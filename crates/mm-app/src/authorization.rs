//! Port of `channels/app/authorization.go` — the core of the permission check.
//!
//! This is the file [D-094] pointed at, and the reason that entry is now **CLOSED**: 674
//! `SessionHasPermission*` call sites across 59 api4 files reach one of these, and every route
//! that needed one used to be forwarded to the Go server. The model layer (`permission.rs`,
//! `role.rs`, `scheme.rs`) and the store layer (`role_store.rs`) were the prerequisites; this is
//! where they meet.
//!
//! # Fail closed
//!
//! `RolesGrantPermission` denies when the role lookup fails, logs, and carries on
//! (authorization.go:386). That is the single most important line in the file: a database blip must
//! not grant. Reproduced exactly, including the log, and asserted by a test that points the store at
//! an unreachable database.
//!
//! # What is ported
//!
//! **20 of the 36 functions in the Go file** — every system-, team-, channel-, user- and
//! post-scoped check, in both the session-scoped and `askingUserId` forms. `GetRolesByNames` and
//! the higher-scoped merge behind it live here too, though they belong to `role.go`. That is the
//! whole surface `api4` reaches for an ordinary read or write of a team, a channel, a user or a
//! post.
//!
//! What remains needs a store this project does not have, and each is named in [D-134]:
//! `…ToGroup` (group store), `…ToCategory` (sidebar-category store), `…ToManageBot` and
//! `…ToUserOrBot` (bot store), the eleven property-field functions (property store), and
//! `HasPermissionToFileAction` (the enterprise ABAC interface, permanently out of scope).
//!
//! # The two families, and why confusing them widens access
//!
//! Every check exists twice: a `session_*` form that reads roles from the **session**, and a bare
//! form that reads them from the **database** for an `asking_user_id`. They are not
//! interchangeable, and the differences are not uniform — the user-scoped forms variously drop
//! the unrestricted branch, the empty-id screen, the `manage_system` shortcut, the
//! existence check and the system-admin-target refusal, while *adding* a `DeleteAt` filter the
//! session form has no need of. Each divergence is documented on the function that has it. The
//! general rule is that **the user-scoped form is weaker**, so substituting it for its twin
//! grants access Go refuses.
//!
//! # Config
//!
//! Two checks read settings that live in the Go server's `config.json`, which this process cannot
//! see. See [`crate::config`] for what is read instead and [D-156] for the divergence.

use mm_model::channel::Channel;
use mm_model::permission::Permission;
use mm_model::role::Role;
use mm_model::session::Session;
use mm_model::utils::AppError;
use mm_store::{ChannelStore, RoleStore, UserStore};

use crate::App;

impl App {
    /// Port of `app.App.GetRolesByNames` (role.go:81) together with
    /// `Server.mergeChannelHigherScopedPermissions` (role.go:111), which it always calls.
    ///
    /// The merge is not optional decoration. For a **scheme-managed** role the stored permission
    /// list is not the effective one: the channel scheme's role is merged against its higher scope,
    /// which can *remove* a moderated permission the row still lists. Skipping it would over-grant
    /// exactly where channel moderation is supposed to bite.
    ///
    /// Go queries only when at least one returned role is scheme-managed (role.go:120), which is
    /// worth keeping — on Team Edition no scheme exists, so the second query is pure cost.
    ///
    /// # Errors
    /// A store failure becomes `app.role.get_by_names.app_error`, 500, as in Go.
    #[tracing::instrument(skip(self))]
    pub async fn get_roles_by_names(&self, names: &[String]) -> Result<Vec<Role>, AppError> {
        let mut roles = self
            .store()
            .role()
            .get_by_names(names)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "role lookup failed");
                AppError::new(
                    "GetRolesByNames",
                    "app.role.get_by_names.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        let scheme_managed: Vec<String> = roles
            .iter()
            .filter(|role| role.scheme_managed)
            .map(|role| role.name.clone())
            .collect();

        if scheme_managed.is_empty() {
            return Ok(roles);
        }

        let higher_scoped = self
            .store()
            .role()
            .channel_higher_scoped_permissions(&scheme_managed)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "higher-scoped permission lookup failed");
                AppError::new(
                    "mergeChannelHigherScopedPermissions",
                    "app.role.get_by_names.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        for role in &mut roles {
            if !role.scheme_managed {
                continue;
            }
            // Go looks the role up by name and merges only on a hit (role.go:131). A scheme-managed
            // role with no higher scope — every built-in one on Team Edition — is left alone.
            if let Some(permissions) = higher_scoped.get(&role.name) {
                role.merge_channel_higher_scoped_permissions(permissions);
            }
        }

        Ok(roles)
    }

    /// Port of `app.App.RolesGrantPermission` (authorization.go:383).
    ///
    /// Two things a reading gets wrong, both of which fail *open* if reproduced carelessly:
    ///
    /// - **A lookup failure denies.** Go logs and returns false. Returning an error here and having
    ///   the caller `unwrap_or(true)`, or treating "no roles found" as "unrestricted", would turn a
    ///   database blip into a grant.
    /// - **A soft-deleted role grants nothing.** The store deliberately returns deleted rows — see
    ///   the note in `role_store.rs` — and this is the function that has to skip them.
    #[tracing::instrument(skip(self))]
    pub async fn roles_grant_permission(&self, role_names: &[String], permission_id: &str) -> bool {
        let roles = match self.get_roles_by_names(role_names).await {
            Ok(roles) => roles,
            Err(err) => {
                // Go: "This should only happen if something is very broken. We can't realistically
                // recover the situation, so deny permission and log an error."
                tracing::error!(
                    error = %err,
                    roles = role_names.join(","),
                    "failed to get roles from database"
                );
                return false;
            }
        };

        roles.iter().any(|role| {
            role.delete_at == 0
                && role
                    .permissions
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .any(|permission| permission == permission_id)
        })
    }

    /// Port of `app.App.HasPermissionTo` (authorization.go:295) — the **user-based** check.
    ///
    /// Not `SessionHasPermissionTo` with a lookup bolted on: this one reads the **user row's**
    /// roles fresh from the store, never the session's copy, and it has no `is_unrestricted`
    /// shortcut — a local-mode session asking through this path gets the row's answer. Any
    /// `GetUser` failure is a quiet `false` (Go discards the error), which means a broken
    /// database *denies* here where most checks would 500 — the caller decides what a denial
    /// means.
    #[tracing::instrument(skip(self), fields(user_id = %asking_user_id))]
    pub async fn has_permission_to(&self, asking_user_id: &str, permission: &Permission) -> bool {
        let Ok(user) = self.get_user(asking_user_id).await else {
            return false;
        };
        self.roles_grant_permission(&owned(user.get_roles()), &permission.id)
            .await
    }

    /// Port of `app.App.SessionHasPermissionTo` (authorization.go:18).
    #[tracing::instrument(skip(self, session), fields(user_id = %session.user_id))]
    pub async fn session_has_permission_to(
        &self,
        session: &Session,
        permission: &Permission,
    ) -> bool {
        if session.is_unrestricted() {
            return true;
        }
        self.roles_grant_permission(&owned(session.get_user_roles()), &permission.id)
            .await
    }

    /// Port of `app.App.SessionHasPermissionToAndNotRestrictedAdmin` (authorization.go:27).
    ///
    /// Not `session_has_permission_to` with an extra condition: when
    /// `ExperimentalSettings.RestrictSystemAdmin` is on this **denies outright** rather than
    /// falling through to the role check, so a caller who would otherwise pass on their own
    /// non-admin roles is denied too. The unrestricted branch still wins, and Go's comment says
    /// why — "a local session is always unrestricted", so the socket API is never restricted by
    /// this setting.
    ///
    /// The setting is read from [`crate::config::Config`], which cannot see the Go server's
    /// `config.json`; see that module and [D-156] for what that costs.
    #[tracing::instrument(skip(self, session), fields(user_id = %session.user_id))]
    pub async fn session_has_permission_to_and_not_restricted_admin(
        &self,
        session: &Session,
        permission: &Permission,
    ) -> bool {
        if session.is_unrestricted() {
            return true;
        }

        if self.config().restrict_system_admin {
            return false;
        }

        self.roles_grant_permission(&owned(session.get_user_roles()), &permission.id)
            .await
    }

    /// Port of `app.App.SessionHasPermissionToAny` (authorization.go:39) — true if **any** of the
    /// permissions is granted, and short-circuiting on the first that is.
    pub async fn session_has_permission_to_any(
        &self,
        session: &Session,
        permissions: &[&Permission],
    ) -> bool {
        for permission in permissions {
            if self.session_has_permission_to(session, permission).await {
                return true;
            }
        }
        false
    }

    /// Port of `app.App.SessionHasPermissionToTeam` (authorization.go:48).
    ///
    /// **The empty-team check comes first, before the unrestricted one**, so an unrestricted session
    /// asking about `""` is still denied. Then the team membership's roles are tried, and the user's
    /// system roles are the fallback — a system admin passes even with no membership.
    #[tracing::instrument(skip(self, session), fields(user_id = %session.user_id))]
    pub async fn session_has_permission_to_team(
        &self,
        session: &Session,
        team_id: &str,
        permission: &Permission,
    ) -> bool {
        if team_id.is_empty() {
            return false;
        }
        if session.is_unrestricted() {
            return true;
        }

        if let Some(team_member) = session.get_team_by_team_id(team_id) {
            if self
                .roles_grant_permission(&owned(team_member.get_roles()), &permission.id)
                .await
            {
                return true;
            }
        }

        self.roles_grant_permission(&owned(session.get_user_roles()), &permission.id)
            .await
    }

    /// Port of `app.App.SessionHasPermissionToTeams` (authorization.go:67) — **all** of them.
    ///
    /// Three things invert the singular check's intuitions:
    ///
    /// 1. **An empty list grants.** Vacuous "all", where
    ///    [`Self::session_has_permission_to_any`]'s empty list *denies* on a vacuous "any". The
    ///    two neighbouring plural helpers therefore disagree on the empty case, in the direction
    ///    each name implies, and a reader who assumes one for the other is wrong about a grant.
    /// 2. **An empty id anywhere in the list denies the whole call**, checked up front over the
    ///    entire slice — not per-iteration. A list of nine good ids and one `""` is a denial, and
    ///    it is a denial *before* the system-permission shortcut below, so even a system admin is
    ///    refused.
    /// 3. **The system check runs once, before the loop.** `SessionHasPermissionTo` — so it
    ///    carries its own unrestricted branch — and on success no team is consulted at all.
    ///
    /// The loop itself grants only when *every* team either yields a membership whose roles grant
    /// or... nothing. There is no per-team fallback to system roles: that was spent before the
    /// loop, so a membership miss inside the loop is an immediate denial.
    #[tracing::instrument(skip(self, session), fields(user_id = %session.user_id, teams = team_ids.len()))]
    pub async fn session_has_permission_to_teams(
        &self,
        session: &Session,
        team_ids: &[String],
        permission: &Permission,
    ) -> bool {
        if team_ids.is_empty() {
            return true;
        }

        if team_ids.iter().any(String::is_empty) {
            return false;
        }

        if self.session_has_permission_to(session, permission).await {
            return true;
        }

        for team_id in team_ids {
            let Some(team_member) = session.get_team_by_team_id(team_id) else {
                return false;
            };
            if !self
                .roles_grant_permission(&owned(team_member.get_roles()), &permission.id)
                .await
            {
                return false;
            }
        }

        true
    }

    /// Port of `app.App.SessionHasPermissionToChannel` (authorization.go:101).
    ///
    /// Returns `(has_permission, is_member)`. The second value is **not** a by-product: Go's
    /// comment on the sibling `HasPermissionToChannel` says it is "used for auditing access without
    /// membership", so a caller can record that a system admin read a channel they do not belong
    /// to. Dropping it would silently remove that signal from every audit record.
    ///
    /// # The branch order, which is not the obvious one
    ///
    /// 1. **An empty channel id denies** — before the unrestricted check, so even an unrestricted
    ///    session is denied for `""`. Same shape as `session_has_permission_to_team`.
    /// 2. **The channel must exist**, and it is fetched *before* the unrestricted check too. A
    ///    404 and a broken lookup both deny; only the second is logged. So an unrestricted session
    ///    asking about a channel that does not exist is denied — the existence check is not a
    ///    formality that a privileged caller skips.
    /// 3. Unrestricted grants.
    /// 4. **Channel membership roles**, from the *plural* store read — see below.
    /// 5. **`manage_system` on the session's user roles.** Note this is checked directly rather
    ///    than through `session_has_permission_to`, so it does not re-run the unrestricted branch.
    /// 6. **Fall back to the team**, when the channel has one; otherwise to the system check. A DM
    ///    or GM has an empty `TeamId` and takes the second path.
    ///
    /// A store failure at step 4 is **swallowed** (Go's `if err == nil`): the check carries on to
    /// the system and team fallbacks rather than denying outright. That is Go's behaviour and it is
    /// reproduced, but it is worth naming — it is the one place in this file where a database
    /// failure does not immediately deny, and the reason it is still safe is that every remaining
    /// branch has to grant on its own evidence.
    ///
    /// # Why the roles come from the plural read
    ///
    /// Go calls `GetAllChannelMembersForUser`, not `GetMember`, and its comment says why: the
    /// former is cache-backed and this is a hot path. That is not a detail we could optimise away,
    /// because **the two store methods resolve roles differently** — see
    /// `process_all_channel_member_roles` in `mm-store` and [D-142]. Substituting `GetMember`
    /// here would change which role names reach `roles_grant_permission` for any member holding a
    /// literal scheme role id in the `Roles` column.
    #[tracing::instrument(skip(self, session), fields(user_id = %session.user_id))]
    pub async fn session_has_permission_to_channel(
        &self,
        session: &Session,
        channel_id: &str,
        permission: &Permission,
    ) -> (bool, bool) {
        if channel_id.is_empty() {
            return (false, false);
        }

        let channel = match self.get_channel(channel_id).await {
            Ok(channel) => channel,
            Err(err) => {
                // Go logs only the non-404 case (authorization.go:110); a missing channel is
                // ordinary and would otherwise fill the log with noise from probing clients.
                if err.status_code != 404 {
                    tracing::warn!(channel_id = %channel_id, error = %err, "Failed to get channel");
                }
                return (false, false);
            }
        };

        if session.is_unrestricted() {
            return (true, false);
        }

        let mut is_member = false;
        // Go passes `includeDeleted = true` here, so membership of an archived channel still
        // grants. Passing false would deny every check on an archived channel, which is not what
        // archiving means: the channel becomes read-only, not invisible to its members.
        if let Ok(members) = self
            .store()
            .channel()
            .get_all_channel_members_for_user(&session.user_id, true)
            .await
        {
            if let Some(roles) = members.get(channel_id) {
                is_member = true;
                let channel_roles = fields(roles);
                if self
                    .roles_grant_permission(&channel_roles, &permission.id)
                    .await
                {
                    return (true, is_member);
                }
            }
        }

        if self
            .roles_grant_permission(
                &owned(session.get_user_roles()),
                &mm_model::permission::PERMISSION_MANAGE_SYSTEM.id,
            )
            .await
        {
            return (true, is_member);
        }

        if !channel.team_id.is_empty() {
            return (
                self.session_has_permission_to_team(session, &channel.team_id, permission)
                    .await,
                is_member,
            );
        }

        (
            self.session_has_permission_to(session, permission).await,
            is_member,
        )
    }

    /// Port of `app.App.SessionHasPermissionToChannels` (authorization.go:143) — **all** of them.
    ///
    /// The plural channel check is *not* the plural team check with a different noun, and the
    /// divergence is worth stating because it decides a grant:
    ///
    /// - **The privileged shortcut comes first**, before any validation. `is_unrestricted` or
    ///   `manage_system` grants without looking at a single id — so an unrestricted session asking
    ///   about `[""]` is **granted here and denied by
    ///   [`Self::session_has_permission_to_teams`]**, which screens empty ids up front. Same
    ///   caller, same malformed input, opposite answers, both Go's.
    /// - **Every channel must exist**, checked in its own pass before any permission work. One
    ///   missing channel denies the whole call. The empty-id check lives inside that pass, which
    ///   is why it sits behind the shortcut.
    /// - `manage_system` is read off the session's user roles **directly**, not via
    ///   `session_has_permission_to`, exactly as the singular channel check does.
    ///
    /// After that it mirrors the singular check's tail: the permission itself system-wide, then
    /// per-channel membership roles from the plural store read, with a store failure denying
    /// every channel (Go's `if err == nil` guard sits inside the loop, so an error falls to the
    /// `return false`).
    #[tracing::instrument(skip(self, session), fields(user_id = %session.user_id, channels = channel_ids.len()))]
    pub async fn session_has_permission_to_channels(
        &self,
        session: &Session,
        channel_ids: &[String],
        permission: &Permission,
    ) -> bool {
        if channel_ids.is_empty() {
            return true;
        }

        if session.is_unrestricted()
            || self
                .roles_grant_permission(
                    &owned(session.get_user_roles()),
                    &mm_model::permission::PERMISSION_MANAGE_SYSTEM.id,
                )
                .await
        {
            return true;
        }

        // Go's own comment: "make sure all channels exist, otherwise return false." Any error
        // denies, not just a 404 — a broken lookup is not a grant.
        for channel_id in channel_ids {
            if channel_id.is_empty() {
                return false;
            }
            if self.get_channel(channel_id).await.is_err() {
                return false;
            }
        }

        if self.session_has_permission_to(session, permission).await {
            return true;
        }

        let members = self
            .store()
            .channel()
            .get_all_channel_members_for_user(&session.user_id, true)
            .await;

        for channel_id in channel_ids {
            let Ok(members) = members.as_ref() else {
                // Go's `if err == nil` guard wraps the whole body, so a store failure reaches
                // the loop's unconditional `return false` on the first channel.
                return false;
            };
            let Some(roles) = members.get(channel_id) else {
                return false;
            };
            if !self
                .roles_grant_permission(&fields(roles), &permission.id)
                .await
            {
                return false;
            }
        }

        true
    }

    /// Port of `app.App.SessionHasPermissionToUser` (authorization.go:250).
    ///
    /// This is the self-shortcut [D-094] names as the reason the four migrated `me`-scoped routes
    /// were portable without any of this machinery. The order matters and is not intuitive:
    ///
    /// 1. an empty target denies, even for an unrestricted session;
    /// 2. unrestricted, or holding `manage_system`, grants — **before** the self check;
    /// 3. acting on oneself grants;
    /// 4. otherwise `edit_other_users` is required;
    /// 5. and even with it, **a system admin target denies**. A lookup failure also denies.
    #[tracing::instrument(skip(self, session), fields(actor = %session.user_id))]
    pub async fn session_has_permission_to_user(&self, session: &Session, user_id: &str) -> bool {
        if user_id.is_empty() {
            return false;
        }
        if session.is_unrestricted()
            || self
                .session_has_permission_to(session, &mm_model::permission::PERMISSION_MANAGE_SYSTEM)
                .await
        {
            return true;
        }

        if session.user_id == user_id {
            return true;
        }

        if !self
            .session_has_permission_to(session, &mm_model::permission::PERMISSION_EDIT_OTHER_USERS)
            .await
        {
            return false;
        }

        // Go swallows the error and denies (authorization.go:270); a missing target is not a grant.
        let Ok(user) = self.store().user().get(user_id).await else {
            return false;
        };

        !user.is_system_admin()
    }

    // ---------------------------------------------------------------------------------------
    // The `askingUserId` family (authorization.go:295-381, :466-512).
    //
    // These differ from their session-scoped twins in one way that decides real answers: they
    // load roles from the **database** rather than from the session, so they see a role change a
    // live session has not picked up yet. They also have **no `is_unrestricted` branch** — a
    // local-mode caller reaching one of these gets the row's answer, not a free pass. Every
    // shortcut the session variants take is absent unless noted, and the omissions are not
    // symmetric; each is called out below.
    // ---------------------------------------------------------------------------------------

    /// Port of `app.App.HasPermissionToTeam` (authorization.go:306).
    ///
    /// The user-scoped twin of [`Self::session_has_permission_to_team`], and it carries one
    /// predicate the session variant has no need of: **`DeleteAt == 0`**. A session's
    /// `TeamMembers` were filtered when the session was built, but this reads the row directly,
    /// so a *departed* member's roles are still on the row and must not grant. Dropping that
    /// check would let someone who left a team keep whatever their team role granted.
    ///
    /// Both ids are screened, and the fallback is the user's system roles via
    /// [`Self::has_permission_to`]. Go discards the `GetTeamMember` error, so a missing
    /// membership and a broken lookup both fall through to that fallback rather than denying.
    #[tracing::instrument(skip(self), fields(user_id = %asking_user_id, team_id = %team_id))]
    pub async fn has_permission_to_team(
        &self,
        asking_user_id: &str,
        team_id: &str,
        permission: &Permission,
    ) -> bool {
        if team_id.is_empty() || asking_user_id.is_empty() {
            return false;
        }

        if let Ok(team_member) = self.get_team_member(team_id, asking_user_id).await {
            if team_member.delete_at == 0
                && self
                    .roles_grant_permission(&owned(team_member.get_roles()), &permission.id)
                    .await
            {
                return true;
            }
        }

        self.has_permission_to(asking_user_id, permission).await
    }

    /// Port of `app.App.HasPermissionToChannel` (authorization.go:327).
    ///
    /// Returns `(has_permission, is_member)`, the same audit signal
    /// [`Self::session_has_permission_to_channel`] carries.
    ///
    /// # Three branches the session variant has and this one does not
    ///
    /// 1. **No existence check up front.** The session variant fetches the channel *first* and
    ///    denies if it is missing. Here membership is consulted first and the channel is fetched
    ///    only to find its team — so a **missing channel does not deny**, it falls through to the
    ///    user's system roles. A system admin therefore passes this check for a channel id that
    ///    does not exist, and is denied by the session variant for the same id.
    /// 2. **No `manage_system` shortcut.** The session variant grants outright on it; here it
    ///    only helps by way of whatever the final `has_permission_to` fallback grants.
    /// 3. **No unrestricted branch**, as with the whole family.
    ///
    /// What is shared: the roles come from the *plural* store read for the reason Go's comment
    /// gives — it is cache-backed and this is hot — and that read resolves scheme roles
    /// differently from `GetMember` ([D-142]), so the substitution is not available here either.
    /// A store failure is swallowed and the check continues, exactly as in the session variant.
    #[tracing::instrument(skip(self), fields(user_id = %asking_user_id, channel_id = %channel_id))]
    pub async fn has_permission_to_channel(
        &self,
        asking_user_id: &str,
        channel_id: &str,
        permission: &Permission,
    ) -> (bool, bool) {
        if channel_id.is_empty() || asking_user_id.is_empty() {
            return (false, false);
        }

        let mut is_member = false;
        if let Ok(members) = self
            .store()
            .channel()
            .get_all_channel_members_for_user(asking_user_id, true)
            .await
        {
            if let Some(roles) = members.get(channel_id) {
                is_member = true;
                if self
                    .roles_grant_permission(&fields(roles), &permission.id)
                    .await
                {
                    return (true, is_member);
                }
            }
        }

        if let Ok(channel) = self.get_channel(channel_id).await {
            if !channel.team_id.is_empty() {
                return (
                    self.has_permission_to_team(asking_user_id, &channel.team_id, permission)
                        .await,
                    is_member,
                );
            }
        }

        (
            self.has_permission_to(asking_user_id, permission).await,
            is_member,
        )
    }

    /// Port of `app.App.HasPermissionToUser` (authorization.go:371).
    ///
    /// **Far weaker than [`Self::session_has_permission_to_user`], and deliberately so.** The
    /// session variant screens an empty target, checks `manage_system`, and — even holding
    /// `edit_other_users` — refuses to act on a system admin. This one does none of that:
    ///
    /// - **No empty-id screen.** `has_permission_to_user("", "")` is `true`, because the two are
    ///   equal. That is Go's answer and it is a trap: the self-check is the *first* line, so any
    ///   two identical strings pass, including two empty ones.
    /// - **No system-admin-target refusal.** Holding `edit_other_users` is enough here, where the
    ///   session variant would still deny for an admin target.
    ///
    /// Callers that want the stronger rule must use the session variant; this is not a drop-in
    /// substitute for it and swapping them would widen access.
    #[tracing::instrument(skip(self), fields(actor = %asking_user_id, target = %user_id))]
    pub async fn has_permission_to_user(&self, asking_user_id: &str, user_id: &str) -> bool {
        if asking_user_id == user_id {
            return true;
        }

        self.has_permission_to(
            asking_user_id,
            &mm_model::permission::PERMISSION_EDIT_OTHER_USERS,
        )
        .await
    }

    // ---------------------------------------------------------------------------------------
    // The read-channel family (authorization.go:450-512). Three checks over the same shape —
    // "member content access, else a public-channel fallback on the team" — differing only in
    // which team permission the fallback names and whether compliance can switch it off.
    // ---------------------------------------------------------------------------------------

    /// Port of `app.App.SessionHasPermissionToReadChannel` (authorization.go:450).
    ///
    /// The unrestricted branch returns `(true, false)` — **`is_member` is `false` even for an
    /// actual member**, because the check short-circuits before any membership read. An audit
    /// record built from this pair will show a local-mode read as non-member access regardless of
    /// the truth; that is Go's behaviour and the audit trail inherits it.
    #[tracing::instrument(skip(self, session, channel), fields(user_id = %session.user_id, channel_id = %channel.id))]
    pub async fn session_has_permission_to_read_channel(
        &self,
        session: &Session,
        channel: &Channel,
    ) -> (bool, bool) {
        if session.is_unrestricted() {
            return (true, false);
        }
        self.has_permission_to_read_channel(&session.user_id, channel)
            .await
    }

    /// Port of `app.App.HasPermissionToReadChannel` (authorization.go:466).
    ///
    /// Content access first, via `read_channel_content` on the channel. Failing that, an **open**
    /// channel falls back to `read_public_channel` on its team — but only while compliance is
    /// off, because a compliance deployment needs every read to come from a membership it can
    /// export.
    ///
    /// Two details that a careless port loses:
    ///
    /// - **The fallback discards `is_member`** and returns `false` for it, even when the first
    ///   branch established the caller *is* a member (it can: membership without
    ///   `read_channel_content` reaches here). Preserving the real value would be more truthful
    ///   and would diverge from Go's audit record.
    /// - **`ChannelTypeOpenBoard` counts as open**, alongside `ChannelTypeOpen`. Matching only
    ///   `"O"` would deny every board read that relies on the public fallback.
    ///
    /// The compliance flag comes from [`crate::config::Config`] and cannot be read from the Go
    /// server's config file — see [D-156]. Getting it wrong **over-grants** here, since `false`
    /// is the branch that opens the fallback.
    #[tracing::instrument(skip(self, channel), fields(user_id = %user_id, channel_id = %channel.id))]
    pub async fn has_permission_to_read_channel(
        &self,
        user_id: &str,
        channel: &Channel,
    ) -> (bool, bool) {
        let (has_permission, is_member) = self
            .has_permission_to_channel(
                user_id,
                &channel.id,
                &mm_model::permission::PERMISSION_READ_CHANNEL_CONTENT,
            )
            .await;
        if has_permission {
            return (true, is_member);
        }

        if is_open_channel(channel) && !self.config().compliance_enable {
            return (
                self.has_permission_to_team(
                    user_id,
                    &channel.team_id,
                    &mm_model::permission::PERMISSION_READ_PUBLIC_CHANNEL,
                )
                .await,
                false,
            );
        }

        (false, false)
    }

    /// Port of `app.App.HasPermissionToResolveChannelMention` (authorization.go:488).
    ///
    /// [`Self::has_permission_to_read_channel`] **without the compliance condition**, and Go's
    /// comment explains why at length: this exposes only a public channel's *name* and link,
    /// never content, and a public channel name is already discoverable within its team through
    /// browse, search and autocomplete. Following the link still requires joining, which is what
    /// creates the compliance trail.
    ///
    /// So a `~mention` resolves on a compliance deployment where a content read would not. Adding
    /// the compliance check here "for consistency" would be a behavioural divergence, not a
    /// tightening.
    #[tracing::instrument(skip(self, channel), fields(user_id = %user_id, channel_id = %channel.id))]
    pub async fn has_permission_to_resolve_channel_mention(
        &self,
        user_id: &str,
        channel: &Channel,
    ) -> bool {
        let (has_permission, _) = self
            .has_permission_to_channel(
                user_id,
                &channel.id,
                &mm_model::permission::PERMISSION_READ_CHANNEL_CONTENT,
            )
            .await;
        if has_permission {
            return true;
        }

        if is_open_channel(channel) {
            return self
                .has_permission_to_team(
                    user_id,
                    &channel.team_id,
                    &mm_model::permission::PERMISSION_READ_PUBLIC_CHANNEL,
                )
                .await;
        }

        false
    }

    /// Port of `app.App.HasPermissionToChannelMemberCount` (authorization.go:500).
    ///
    /// The third member of the family: same shape, no compliance condition, and the team fallback
    /// names **`list_team_channels`** rather than `read_public_channel`. The two permissions are
    /// held by different roles, so substituting one for the other changes who can count a public
    /// channel's members without joining it.
    #[tracing::instrument(skip(self, channel), fields(user_id = %user_id, channel_id = %channel.id))]
    pub async fn has_permission_to_channel_member_count(
        &self,
        user_id: &str,
        channel: &Channel,
    ) -> bool {
        let (has_permission, _) = self
            .has_permission_to_channel(
                user_id,
                &channel.id,
                &mm_model::permission::PERMISSION_READ_CHANNEL_CONTENT,
            )
            .await;
        if has_permission {
            return true;
        }

        if is_open_channel(channel) {
            return self
                .has_permission_to_team(
                    user_id,
                    &channel.team_id,
                    &mm_model::permission::PERMISSION_LIST_TEAM_CHANNELS,
                )
                .await;
        }

        false
    }

    // ---------------------------------------------------------------------------------------
    // The by-post family (authorization.go:207-240, :357). A post id stands in for a channel id;
    // both store reads join through `Posts` and neither needs a post *model*, which is why these
    // are portable with no post store. [D-134] listed them as blocked on one — they were not.
    // ---------------------------------------------------------------------------------------

    /// Port of `app.App.SessionHasPermissionToChannelByPost` (authorization.go:207).
    ///
    /// Membership of the post's channel, then the post's team, then the system — and **each step
    /// falls through on a store error rather than denying**, because Go guards every one with
    /// `if err == nil`. A post that does not exist therefore reaches the final system check, so a
    /// system admin is granted for a post id that was never real.
    ///
    /// Note the middle branch's guard: the team fallback runs **only when the channel has a
    /// team**. A DM or GM post has an empty `TeamId` and drops to the system check instead —
    /// which is the opposite of what [`Self::has_permission_to_channel_by_post`] does with the
    /// same input. See that function.
    #[tracing::instrument(skip(self, session), fields(user_id = %session.user_id, post_id = %post_id))]
    pub async fn session_has_permission_to_channel_by_post(
        &self,
        session: &Session,
        post_id: &str,
        permission: &Permission,
    ) -> bool {
        if post_id.is_empty() {
            return false;
        }

        if let Ok(channel_member) = self
            .store()
            .channel()
            .get_member_for_post(post_id, &session.user_id)
            .await
        {
            if self
                .roles_grant_permission(&owned(channel_member.get_roles()), &permission.id)
                .await
            {
                return true;
            }
        }

        if let Ok(channel) = self.store().channel().get_for_post(post_id).await {
            if !channel.team_id.is_empty() {
                return self
                    .session_has_permission_to_team(session, &channel.team_id, permission)
                    .await;
            }
        }

        self.session_has_permission_to(session, permission).await
    }

    /// Port of `app.App.HasPermissionToChannelByPost` (authorization.go:357).
    ///
    /// The user-scoped twin, and it is **not** the session variant with a different role source.
    /// Two divergences, both of which change the answer:
    ///
    /// - **No empty-post-id screen.** Go checks `postID == ""` in the session variant and not
    ///   here, so an empty id falls through to the store reads (which miss) and then to the
    ///   user's system roles — a grant the session variant refuses outright.
    /// - **No `TeamId != ""` guard.** Where the session variant skips the team fallback for a
    ///   channel with no team, this calls `HasPermissionToTeam` unconditionally — and that
    ///   function screens an empty team id and returns `false`. So for a **DM or GM post the two
    ///   twins disagree**: the session variant falls through to the system check and can grant,
    ///   this one denies. Adding the guard "for symmetry" would be a divergence.
    #[tracing::instrument(skip(self), fields(user_id = %asking_user_id, post_id = %post_id))]
    pub async fn has_permission_to_channel_by_post(
        &self,
        asking_user_id: &str,
        post_id: &str,
        permission: &Permission,
    ) -> bool {
        if let Ok(channel_member) = self
            .store()
            .channel()
            .get_member_for_post(post_id, asking_user_id)
            .await
        {
            if self
                .roles_grant_permission(&owned(channel_member.get_roles()), &permission.id)
                .await
            {
                return true;
            }
        }

        if let Ok(channel) = self.store().channel().get_for_post(post_id).await {
            // Unconditional — no `TeamId != ""` guard. An empty team id is screened by
            // `has_permission_to_team` itself and denies there.
            return self
                .has_permission_to_team(asking_user_id, &channel.team_id, permission)
                .await;
        }

        self.has_permission_to(asking_user_id, permission).await
    }

    /// Port of `app.App.SessionHasPermissionToReadPost` (authorization.go:227).
    ///
    /// Resolves the post's channel and defers to
    /// [`Self::session_has_permission_to_read_channel`]. When the channel cannot be resolved it
    /// falls back to a bare `read_channel_content` system check — and Go's own comment says this
    /// is deliberate rather than an oversight: "the original implementation still checks for
    /// general permissions even if the channel is not found, and some tests rely on this
    /// behavior."
    ///
    /// The fallback's `is_member` is `false`, as is the empty-id denial's.
    #[tracing::instrument(skip(self, session), fields(user_id = %session.user_id, post_id = %post_id))]
    pub async fn session_has_permission_to_read_post(
        &self,
        session: &Session,
        post_id: &str,
    ) -> (bool, bool) {
        if post_id.is_empty() {
            return (false, false);
        }

        let Ok(channel) = self.store().channel().get_for_post(post_id).await else {
            return (
                self.session_has_permission_to(
                    session,
                    &mm_model::permission::PERMISSION_READ_CHANNEL_CONTENT,
                )
                .await,
                false,
            );
        };

        self.session_has_permission_to_read_channel(session, &channel)
            .await
    }
}

/// Go's `channel.Type == ChannelTypeOpen || channel.Type == ChannelTypeOpenBoard`, the condition
/// all three read-channel checks share (authorization.go:475, :494, :506).
///
/// Extracted rather than inlined three times so the mutation harness has a single point to
/// attack: three copies of a two-arm condition are three independent chances to drop the
/// `OpenBoard` arm, and a test suite that pins only one call site would not notice.
fn is_open_channel(channel: &Channel) -> bool {
    channel.channel_type == mm_model::channel::CHANNEL_TYPE_OPEN
        || channel.channel_type == mm_model::channel::CHANNEL_TYPE_OPEN_BOARD
}

/// `Session::get_user_roles` and `TeamMember::get_roles` borrow out of the session, while the store
/// takes owned names because the query binds a `text[]`. One allocation per check, at the boundary
/// where Go also materialises a slice.
fn owned(roles: Vec<&str>) -> Vec<String> {
    roles.into_iter().map(str::to_owned).collect()
}

/// Go's `strings.Fields` over a stored role string. Splits on runs of whitespace and drops
/// empties, so an empty `Roles` value yields no names rather than one empty name — which would
/// otherwise be looked up as a role called `""`.
fn fields(roles: &str) -> Vec<String> {
    roles.split_whitespace().map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_model::permission::{PERMISSION_CREATE_POST, PERMISSION_MANAGE_SYSTEM};
    use mm_model::team_member::TeamMember;
    use mm_store::SqlStore;
    use sqlx::postgres::PgPoolOptions;

    /// An `App` whose store points at a database that cannot be reached, and is never connected to
    /// because `connect_lazy` defers the attempt to first use.
    ///
    /// That makes it an assertion rather than a stub: any check that returns `true` here **proved
    /// it never touched the store**, and any check that reaches the store denies, which is exactly
    /// the fail-closed behaviour under test.
    fn app_with_unreachable_store() -> App {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            // `acquire_timeout` is set because sqlx's default is **30 seconds**, and the connection
            // to :1 is refused instantly but retried until that window expires. Six tests wearing that
            // default cost 90 seconds of every `cargo test -p mm-app`, and 6 minutes under
            // `--test-threads=1`. The error a caller sees is `PoolTimedOut` either way, so nothing
            // under test changes — only how long we wait to see it.
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody:nobody@127.0.0.1:1/nonexistent")
            .expect("a lazy pool never connects");
        App::new(SqlStore::from_pool(pool))
    }

    fn session(user_id: &str, roles: &str) -> Session {
        Session {
            user_id: user_id.to_owned(),
            roles: roles.to_owned(),
            ..Default::default()
        }
    }

    /// `HasPermissionTo` reads the **user row**, so a broken store is a quiet `false` — Go
    /// discards the `GetUser` error (authorization.go:298). Fail-closed, like everything here:
    /// the caller (`getTeamStats`'s restrictions fast path) treats the denial as "forward to
    /// Go", which is the conservative direction.
    #[tokio::test]
    async fn the_user_based_check_denies_when_the_user_cannot_be_loaded() {
        let app = app_with_unreachable_store();
        assert!(
            !app.has_permission_to("y9i4er48tt8bukijy7i3u5y9ar", &PERMISSION_MANAGE_SYSTEM)
                .await
        );
    }

    #[tokio::test]
    async fn an_unreachable_store_denies_rather_than_grants() {
        let app = app_with_unreachable_store();
        let session = session("y9i4er48tt8bukijy7i3u5y9ar", "system_admin");

        // The whole point of authorization.go:386. `system_admin` would grant this against a
        // working database; a broken one must not.
        assert!(
            !app.session_has_permission_to(&session, &PERMISSION_CREATE_POST)
                .await
        );
        assert!(
            !app.roles_grant_permission(&["system_admin".to_owned()], "create_post")
                .await
        );
        assert!(
            !app.session_has_permission_to_team(
                &session,
                "teamid1jbyqbtxbtqcgy3wa",
                &PERMISSION_CREATE_POST
            )
            .await
        );
    }

    #[tokio::test]
    async fn the_short_circuits_never_reach_the_store() {
        let app = app_with_unreachable_store();

        // An unrestricted session grants without a lookup — if it consulted the store it would deny,
        // because the store is unreachable.
        let mut local = session("y9i4er48tt8bukijy7i3u5y9ar", "");
        local.is_oauth = false;
        local.local = true;
        assert!(local.is_unrestricted(), "a local session is unrestricted");
        assert!(
            app.session_has_permission_to(&local, &PERMISSION_CREATE_POST)
                .await
        );
        assert!(
            app.session_has_permission_to_team(
                &local,
                "teamid1jbyqbtxbtqcgy3wa",
                &PERMISSION_CREATE_POST
            )
            .await
        );
        assert!(
            app.session_has_permission_to_user(&local, "otheruser1jbyqbtxbtqcgy3")
                .await
        );

        // ...but an empty team id denies even so, because that check comes first.
        assert!(
            !app.session_has_permission_to_team(&local, "", &PERMISSION_CREATE_POST)
                .await
        );
        // And an empty user id denies for the same reason.
        assert!(!app.session_has_permission_to_user(&local, "").await);
    }

    #[tokio::test]
    async fn an_empty_permission_list_denies() {
        let app = app_with_unreachable_store();
        let local = Session {
            local: true,
            ..Default::default()
        };
        // Vacuously false: `any` over nothing. Worth pinning because the natural "all" reading
        // would return true here and grant on an empty list.
        assert!(!app.session_has_permission_to_any(&local, &[]).await);
        assert!(
            app.session_has_permission_to_any(&local, &[&PERMISSION_CREATE_POST])
                .await
        );
    }

    #[tokio::test]
    async fn team_membership_roles_are_consulted_before_user_roles() {
        let app = app_with_unreachable_store();
        let mut session = session("y9i4er48tt8bukijy7i3u5y9ar", "system_user");
        session.team_members = Some(vec![TeamMember {
            team_id: "teamid1jbyqbtxbtqcgy3wa".to_owned(),
            user_id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            roles: "team_admin".to_owned(),
            ..Default::default()
        }]);

        // Both lookups fail against the unreachable store, so this asserts the *denial* path rather
        // than the ordering. The ordering itself is asserted by the DB-backed suite; what this pins
        // is that a membership hit does not skip the fallback and accidentally grant.
        assert!(
            !app.session_has_permission_to_team(
                &session,
                "teamid1jbyqbtxbtqcgy3wa",
                &PERMISSION_MANAGE_SYSTEM
            )
            .await
        );
    }

    // -----------------------------------------------------------------------------------------
    // The plural checks. Their empty-input answers are the highest-value thing to pin: they are
    // constants, they disagree with each other, and both are reachable from a real request body.
    // -----------------------------------------------------------------------------------------

    /// **The two plural helpers disagree on the empty list, and both answers are Go's.**
    ///
    /// `SessionHasPermissionToTeams` and `…ToChannels` are vacuous *all* (authorization.go:68,
    /// :144) and grant; `SessionHasPermissionToAny` is a vacuous *any* (:39) and denies. A reader
    /// who assumes one shape for the whole family is wrong about a grant in one direction or the
    /// other. Asserted against the unreachable store, so a `true` here also proves the empty case
    /// never reaches the database.
    #[tokio::test]
    async fn the_empty_list_grants_for_all_and_denies_for_any() {
        let app = app_with_unreachable_store();
        let session = session("y9i4er48tt8bukijy7i3u5y9ar", "system_user");

        assert!(
            app.session_has_permission_to_teams(&session, &[], &PERMISSION_CREATE_POST)
                .await,
            "vacuous all — authorization.go:68"
        );
        assert!(
            app.session_has_permission_to_channels(&session, &[], &PERMISSION_CREATE_POST)
                .await,
            "vacuous all — authorization.go:144"
        );
        assert!(
            !app.session_has_permission_to_any(&session, &[]).await,
            "vacuous any — authorization.go:39"
        );
    }

    /// **An unrestricted session asking about `[""]` is denied by teams and granted by channels.**
    ///
    /// The same caller, the same malformed input, opposite answers — because
    /// `SessionHasPermissionToTeams` screens empty ids *before* its permission shortcut
    /// (authorization.go:71) while `…ToChannels` screens them *after* (:148, inside the existence
    /// loop). This is the single most surprising pair in the file and neither answer is
    /// incidental.
    #[tokio::test]
    async fn an_empty_id_in_the_list_splits_the_two_plural_checks() {
        let app = app_with_unreachable_store();
        let mut local = session("y9i4er48tt8bukijy7i3u5y9ar", "");
        local.is_oauth = false;
        local.local = true;
        assert!(local.is_unrestricted());

        let ids = vec![String::new()];
        assert!(
            !app.session_has_permission_to_teams(&local, &ids, &PERMISSION_CREATE_POST)
                .await,
            "teams screens the empty id before the shortcut"
        );
        assert!(
            app.session_has_permission_to_channels(&local, &ids, &PERMISSION_CREATE_POST)
                .await,
            "channels takes the unrestricted shortcut before it ever looks at the id"
        );
    }

    /// A non-empty team list with no shortcut has to reach the store, and the unreachable store
    /// denies. Pins that the loop does **not** treat a membership miss as "carry on" — the
    /// `continue` in Go's loop is only for the grant case.
    #[tokio::test]
    async fn a_team_with_no_membership_denies_the_whole_plural_check() {
        let app = app_with_unreachable_store();
        let session = session("y9i4er48tt8bukijy7i3u5y9ar", "system_user");

        // No `team_members` on the session at all, so `get_team_by_team_id` misses and the loop
        // hits its unconditional denial.
        assert!(
            !app.session_has_permission_to_teams(
                &session,
                &["teamid1jbyqbtxbtqcgy3wa".to_owned()],
                &PERMISSION_CREATE_POST
            )
            .await
        );
    }

    // -----------------------------------------------------------------------------------------
    // The `askingUserId` family. Every one of these reaches the store for roles, so against the
    // unreachable store they all deny — which makes this the right place to pin the branches
    // that answer *without* reaching it.
    // -----------------------------------------------------------------------------------------

    /// **`HasPermissionToUser` grants for two empty strings**, because the self-check is its
    /// first line (authorization.go:372) and it has no empty-id screen — unlike its session-scoped
    /// twin, which screens `""` before anything else (:251).
    ///
    /// This is the sharpest asymmetry in the family and it is a grant, so it is pinned in both
    /// directions.
    #[tokio::test]
    async fn the_user_scoped_check_has_no_empty_id_screen_and_its_twin_does() {
        let app = app_with_unreachable_store();

        assert!(
            app.has_permission_to_user("", "").await,
            "two empty strings are equal, so the self-check grants — authorization.go:372"
        );
        assert!(
            app.has_permission_to_user("y9i4er48tt8bukijy7i3u5y9ar", "y9i4er48tt8bukijy7i3u5y9ar")
                .await,
            "the ordinary self case"
        );

        // The session-scoped twin refuses the same empty target outright.
        let mut local = session("y9i4er48tt8bukijy7i3u5y9ar", "");
        local.local = true;
        assert!(!app.session_has_permission_to_user(&local, "").await);
    }

    /// Both ids are screened by `HasPermissionToTeam` (authorization.go:307) and by
    /// `HasPermissionToChannel` (:333) — and screened *before* any store read, so these deny
    /// without the database being reachable.
    #[tokio::test]
    async fn the_user_scoped_team_and_channel_checks_screen_both_ids() {
        let app = app_with_unreachable_store();

        assert!(
            !app.has_permission_to_team("", "teamid1jbyqbtxbtqcgy3wa", &PERMISSION_CREATE_POST)
                .await
        );
        assert!(
            !app.has_permission_to_team("y9i4er48tt8bukijy7i3u5y9ar", "", &PERMISSION_CREATE_POST)
                .await
        );

        assert_eq!(
            app.has_permission_to_channel("", "chanid1jbyqbtxbtqcgy3wa", &PERMISSION_CREATE_POST)
                .await,
            (false, false)
        );
        assert_eq!(
            app.has_permission_to_channel(
                "y9i4er48tt8bukijy7i3u5y9ar",
                "",
                &PERMISSION_CREATE_POST
            )
            .await,
            (false, false)
        );
    }

    /// The unrestricted branch of `SessionHasPermissionToReadChannel` returns
    /// **`(true, false)`** — granted, and `is_member` false even though no membership was ever
    /// checked (authorization.go:456). An audit record built from this pair records a local-mode
    /// read as non-member access.
    #[tokio::test]
    async fn an_unrestricted_read_channel_reports_non_membership() {
        let app = app_with_unreachable_store();
        let mut local = session("y9i4er48tt8bukijy7i3u5y9ar", "");
        local.local = true;

        let channel = Channel {
            id: "chanid1jbyqbtxbtqcgy3wa".to_owned(),
            channel_type: mm_model::channel::CHANNEL_TYPE_OPEN.to_owned(),
            team_id: "teamid1jbyqbtxbtqcgy3wa".to_owned(),
            ..Default::default()
        };

        assert_eq!(
            app.session_has_permission_to_read_channel(&local, &channel)
                .await,
            (true, false)
        );
    }

    /// `SessionHasPermissionToChannelByPost` screens an empty post id (authorization.go:208);
    /// `HasPermissionToChannelByPost` does **not** (:357), so the latter falls through its two
    /// store reads to the system check. Against the unreachable store both end up `false`, but
    /// they arrive there by different routes — the session one without touching the store at all.
    #[tokio::test]
    async fn only_the_session_scoped_by_post_check_screens_an_empty_post_id() {
        let app = app_with_unreachable_store();
        let mut local = session("y9i4er48tt8bukijy7i3u5y9ar", "");
        local.local = true;

        // Unrestricted, so if the empty-id screen were missing the final
        // `session_has_permission_to` would grant. It denies, which proves the screen ran.
        assert!(
            !app.session_has_permission_to_channel_by_post(&local, "", &PERMISSION_CREATE_POST)
                .await
        );

        // Same for the read-post variant (authorization.go:228).
        assert_eq!(
            app.session_has_permission_to_read_post(&local, "").await,
            (false, false)
        );
    }

    /// The `OpenBoard` arm of the open-channel condition. All three read-channel checks share it
    /// (authorization.go:475, :494, :506) and a port that matches only `"O"` denies every board
    /// read that relies on the public fallback.
    ///
    /// Asserted on the helper directly: the checks themselves need a working store to reach the
    /// fallback, and that is what the DB-backed suite is for.
    #[test]
    fn a_board_channel_counts_as_open() {
        let open_types = [
            mm_model::channel::CHANNEL_TYPE_OPEN,
            mm_model::channel::CHANNEL_TYPE_OPEN_BOARD,
        ];
        for channel_type in open_types {
            let channel = Channel {
                channel_type: channel_type.to_owned(),
                ..Default::default()
            };
            assert!(is_open_channel(&channel), "{channel_type} should be open");
        }

        for channel_type in [
            mm_model::channel::CHANNEL_TYPE_PRIVATE,
            mm_model::channel::CHANNEL_TYPE_DIRECT,
            mm_model::channel::CHANNEL_TYPE_GROUP,
            mm_model::channel::CHANNEL_TYPE_PRIVATE_BOARD,
            mm_model::channel::CHANNEL_TYPE_SPACE,
        ] {
            let channel = Channel {
                channel_type: channel_type.to_owned(),
                ..Default::default()
            };
            assert!(
                !is_open_channel(&channel),
                "{channel_type} should not be open"
            );
        }
    }

    /// `RestrictSystemAdmin` **denies outright** rather than falling through to the role check
    /// (authorization.go:32), and the unrestricted branch still wins ahead of it (:28) — Go's
    /// comment says so: "a local session is always unrestricted".
    #[tokio::test]
    async fn restricting_the_system_admin_denies_without_consulting_roles() {
        use crate::config::Config;

        let restricted = App::with_config(
            app_with_unreachable_store().store().clone(),
            Config {
                restrict_system_admin: true,
                ..Config::default()
            },
        );

        // A local session is unrestricted and passes ahead of the setting, without a store read.
        let mut local = session("y9i4er48tt8bukijy7i3u5y9ar", "");
        local.local = true;
        assert!(
            restricted
                .session_has_permission_to_and_not_restricted_admin(&local, &PERMISSION_CREATE_POST)
                .await
        );

        // An ordinary session is denied — and denied *without* a store read, which the
        // unreachable store cannot distinguish on its own. The default-config case below is what
        // makes this meaningful: same session, same store, opposite answer.
        let ordinary = session("y9i4er48tt8bukijy7i3u5y9ar", "system_admin");
        assert!(
            !restricted
                .session_has_permission_to_and_not_restricted_admin(
                    &ordinary,
                    &PERMISSION_CREATE_POST
                )
                .await
        );
    }

    /// With the setting off — Go's default — the check is exactly `SessionHasPermissionTo`, so it
    /// reaches the store and the unreachable store denies. Paired with the test above so that
    /// "denies" is not the answer to every question the config could be asked.
    #[tokio::test]
    async fn the_default_config_leaves_the_restricted_admin_check_reaching_the_store() {
        let app = app_with_unreachable_store();
        assert!(!app.config().restrict_system_admin, "Go's default");

        let mut local = session("y9i4er48tt8bukijy7i3u5y9ar", "");
        local.local = true;
        assert!(
            app.session_has_permission_to_and_not_restricted_admin(&local, &PERMISSION_CREATE_POST)
                .await,
            "unrestricted still short-circuits"
        );
    }
}
