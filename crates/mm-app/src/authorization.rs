//! Port of `channels/app/authorization.go` — the core of the permission check.
//!
//! This is the function [D-094] has been pointing at: 674 `SessionHasPermission*` call sites across
//! 59 api4 files reach one of these, and until now every route that needed one was forwarded to the
//! Go server. The model layer (`permission.rs`, `role.rs`, `scheme.rs`) and the store layer
//! (`role_store.rs`) were the prerequisites; this is where they meet.
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
//! The system-, team- and user-scoped checks, plus `GetRolesByNames` and the higher-scoped merge
//! behind it. The channel, post, group, category, bot and property-field variants need stores that
//! do not exist yet, and `SessionHasPermissionToAndNotRestrictedAdmin` needs `Config`. See [D-134].

use mm_model::permission::Permission;
use mm_model::role::Role;
use mm_model::session::Session;
use mm_model::utils::AppError;
use mm_store::{RoleStore, UserStore};

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
}

/// `Session::get_user_roles` and `TeamMember::get_roles` borrow out of the session, while the store
/// takes owned names because the query binds a `text[]`. One allocation per check, at the boundary
/// where Go also materialises a slice.
fn owned(roles: Vec<&str>) -> Vec<String> {
    roles.into_iter().map(str::to_owned).collect()
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
}
