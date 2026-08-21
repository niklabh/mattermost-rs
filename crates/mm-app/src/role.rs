//! Port of the **read** half of `channels/app/role.go` — `GetRole` (:23), `GetAllRoles` (:42),
//! `Server.GetRoleByName` (:57) and the merge every one of them ends with (:107).
//!
//! # A role on the wire is the database row, never the compiled default
//!
//! `model.MakeDefaultRoles()` (role.go:919) defines all 24 built-in roles in Go source, and it is
//! tempting to read `GetRoleByName("system_user")` as "look up the built-in definition". It is
//! not. Every one of these functions goes straight to `store.Role()` and nothing anywhere in the
//! read path consults `MakeDefaultRoles`; the defaults are used **once, at startup**, to seed and
//! reconcile the `Roles` table. So a row an administrator has patched — `PUT /roles/{id}/patch`
//! rewrites `Permissions` in place — is what every client sees afterwards, and a port that
//! answered from the compiled table would silently un-do every permission change ever made on
//! the server. The parity suite pins this with a role whose row is patched away from its default.
//!
//! Two consequences follow from the same fact:
//!
//! - **A built-in role that has no row does not exist.** There is no fallback; the route 404s.
//! - **`built_in` and `scheme_managed` on the wire are column values**, not properties of the
//!   name. Nothing recomputes them from `IsBuiltInRole`.
//!
//! # `DeleteAt` is not filtered
//!
//! None of these read paths excludes a soft-deleted role — see the note in
//! `mm_store::role_store`. `RolesGrantPermission` is where the `DeleteAt == 0` test lives, so a
//! deleted role is *returned* by these routes while granting nothing.
//!
//! # Where `GetRolesByNames` lives
//!
//! `App::get_roles_by_names` — the third read, and the one `POST /roles/names` calls — is in
//! `authorization.rs`, because the permission check needed it before any route did. It carries
//! its own copy of the merge below rather than calling into this module; the two are the same
//! twelve lines of Go and are tested independently. Folding them together is worth doing and is
//! not worth doing in a session whose sibling worktrees are editing that file.

use mm_model::role::Role;
use mm_model::utils::AppError;
use mm_store::RoleStore;

use crate::App;

impl App {
    /// Port of `app.App.GetRole` (role.go:23) — `GET /api/v4/roles/{role_id}`.
    ///
    /// A missing row is Go's `store.ErrNotFound`, which the app layer turns into **404
    /// `app.role.get.app_error`** — the *same id* it uses for a database failure at 500. Only the
    /// status distinguishes them, so a port that invents a second id breaks any client branching
    /// on `id`.
    #[tracing::instrument(skip(self))]
    pub async fn get_role(&self, id: &str) -> Result<Role, AppError> {
        let role = self.store().role().get(id).await.map_err(|err| {
            tracing::error!(error = %err, "role lookup failed");
            AppError::new(
                "GetRole",
                "app.role.get.app_error",
                None,
                String::new(),
                500,
            )
        })?;

        let mut roles = match role {
            Some(role) => vec![role],
            None => {
                return Err(AppError::new(
                    "GetRole",
                    "app.role.get.app_error",
                    None,
                    String::new(),
                    404,
                ));
            }
        };

        self.merge_channel_higher_scoped_permissions(&mut roles)
            .await?;

        roles.pop().ok_or_else(|| {
            // Unreachable: the vector holds exactly one element and the merge never removes one.
            AppError::new(
                "GetRole",
                "app.role.get.app_error",
                None,
                String::new(),
                500,
            )
        })
    }

    /// Port of `app.Server.GetRoleByName` (role.go:57) — `GET /api/v4/roles/name/{role_name}`.
    ///
    /// Note the error id differs from [`App::get_role`]'s by one word — `get_by_name` rather than
    /// `get` — while the status codes (404 missing, 500 broken) are the same pair.
    #[tracing::instrument(skip(self))]
    pub async fn get_role_by_name(&self, name: &str) -> Result<Role, AppError> {
        let role = self.store().role().get_by_name(name).await.map_err(|err| {
            tracing::error!(error = %err, "role lookup by name failed");
            AppError::new(
                "GetRoleByName",
                "app.role.get_by_name.app_error",
                None,
                String::new(),
                500,
            )
        })?;

        let mut roles = match role {
            Some(role) => vec![role],
            None => {
                return Err(AppError::new(
                    "GetRoleByName",
                    "app.role.get_by_name.app_error",
                    None,
                    String::new(),
                    404,
                ));
            }
        };

        self.merge_channel_higher_scoped_permissions(&mut roles)
            .await?;

        roles.pop().ok_or_else(|| {
            AppError::new(
                "GetRoleByName",
                "app.role.get_by_name.app_error",
                None,
                String::new(),
                500,
            )
        })
    }

    /// Port of `app.App.GetAllRoles` (role.go:42) — `GET /api/v4/roles`.
    ///
    /// No not-found branch: an empty table is an empty list, and only a database failure is an
    /// error. Go's own `where` for it is `GetAllRoles` and the id `app.role.get_all.app_error`,
    /// which nothing else uses.
    #[tracing::instrument(skip(self))]
    pub async fn get_all_roles(&self) -> Result<Vec<Role>, AppError> {
        let mut roles = self.store().role().get_all().await.map_err(|err| {
            tracing::error!(error = %err, "reading every role failed");
            AppError::new(
                "GetAllRoles",
                "app.role.get_all.app_error",
                None,
                String::new(),
                500,
            )
        })?;

        self.merge_channel_higher_scoped_permissions(&mut roles)
            .await?;

        Ok(roles)
    }

    /// Port of `app.Server.mergeChannelHigherScopedPermissions` (role.go:107).
    ///
    /// For a **scheme-managed** role the stored permission list is not the effective one: a
    /// channel scheme's role is recomputed against its higher scope, which can *remove* a
    /// moderated permission the row still lists. Three details a reading gets wrong:
    ///
    /// 1. **Only scheme-managed roles are asked about**, and if none of the roles in hand is
    ///    scheme-managed the second query is skipped entirely (role.go:120). On Team Edition,
    ///    where no channel scheme exists, the map comes back without any of these names in it.
    /// 2. **A scheme-managed role with no entry in the map is left alone** (role.go:131) — its
    ///    permissions stay exactly as stored. That is every built-in role on a stock server, so
    ///    the merge is invisible there; it is not a licence to skip it.
    /// 3. When it *does* fire it **replaces** `Permissions` wholesale with a list rebuilt from
    ///    `AllPermissions` in that global's order — see
    ///    [`Role::merge_channel_higher_scoped_permissions`](mm_model::role::Role::merge_channel_higher_scoped_permissions).
    ///
    /// The error is Go's: `where` is the *merge*'s name, never the caller's, and the id is
    /// `app.role.get_by_names.app_error` even when the caller was `GetRole`.
    async fn merge_channel_higher_scoped_permissions(
        &self,
        roles: &mut [Role],
    ) -> Result<(), AppError> {
        let scheme_managed: Vec<String> = roles
            .iter()
            .filter(|role| role.scheme_managed)
            .map(|role| role.name.clone())
            .collect();

        if scheme_managed.is_empty() {
            return Ok(());
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

        apply_higher_scoped(roles, &higher_scoped);

        Ok(())
    }
}

/// The second loop of `mergeChannelHigherScopedPermissions` (role.go:128-134), split out from its
/// caller so it can be tested at all.
///
/// **No fixture reachable through the REST API can exercise this.** It fires only for a role the
/// higher-scoped query answered about, and that query returns rows only for a role belonging to a
/// *channel scheme* attached to a channel — and creating a scheme needs an enterprise licence.
/// On the development stack the map is always empty, so a mutation that inverts the
/// `scheme_managed` test or drops the loop entirely is invisible to every cross-server test.
/// Hence a free function over an explicit map: the branches are pinned here instead.
fn apply_higher_scoped(
    roles: &mut [Role],
    higher_scoped: &std::collections::BTreeMap<String, mm_model::role::RolePermissions>,
) {
    for role in roles.iter_mut() {
        if !role.scheme_managed {
            continue;
        }
        if let Some(permissions) = higher_scoped.get(&role.name) {
            role.merge_channel_higher_scoped_permissions(permissions);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use mm_model::role::{CHANNEL_USER_ROLE_ID, Role, RolePermissions};

    use super::*;

    /// A role whose row lists one permission the higher scope grants and one it does not.
    fn channel_role(name: &str, scheme_managed: bool) -> Role {
        Role {
            name: name.to_owned(),
            scheme_managed,
            permissions: Some(vec!["create_post".to_owned(), "edit_post".to_owned()]),
            ..Default::default()
        }
    }

    /// The higher scope grants `create_post` (a **moderated** permission, so the role must list it
    /// too) and `read_channel` (not moderated, so the higher scope alone is enough).
    fn higher_scope_for(name: &str) -> BTreeMap<String, RolePermissions> {
        let mut map = BTreeMap::new();
        map.insert(
            name.to_owned(),
            RolePermissions {
                role_id: CHANNEL_USER_ROLE_ID.to_owned(),
                permissions: vec!["create_post".to_owned(), "read_channel".to_owned()],
            },
        );
        map
    }

    /// The merge **replaces** the permission list rather than intersecting or extending it: a
    /// permission the row lists and the higher scope does not is gone, and one only the higher
    /// scope lists is added.
    #[test]
    fn a_scheme_managed_role_in_the_map_is_rebuilt_from_the_higher_scope() {
        let mut roles = [channel_role("custom_channel_user", true)];
        apply_higher_scoped(&mut roles, &higher_scope_for("custom_channel_user"));

        let permissions = roles[0].permissions.clone().expect("a list");
        assert!(
            permissions.contains(&"create_post".to_owned()),
            "moderated, on the row and on the higher scope: kept — {permissions:?}"
        );
        assert!(
            permissions.contains(&"read_channel".to_owned()),
            "not moderated and on the higher scope: added — {permissions:?}"
        );
        assert!(
            !permissions.contains(&"edit_post".to_owned()),
            "on the row but not the higher scope: dropped — {permissions:?}"
        );
        assert_eq!(permissions.len(), 2, "{permissions:?}");
    }

    /// A scheme-managed role the query said nothing about keeps its stored permissions exactly.
    /// This is every built-in role on a stock server, so getting it wrong would empty the
    /// permission list of the whole installation.
    #[test]
    fn a_scheme_managed_role_absent_from_the_map_is_untouched() {
        let mut roles = [channel_role("channel_user", true)];
        apply_higher_scoped(&mut roles, &higher_scope_for("some_other_role"));
        assert_eq!(
            roles[0].permissions,
            Some(vec!["create_post".to_owned(), "edit_post".to_owned()])
        );
    }

    /// A role that is **not** scheme-managed is skipped even when the map happens to name it.
    /// Nothing on Team Edition can produce that map at all, so this branch is unreachable from
    /// any cross-server fixture and is pinned only here.
    #[test]
    fn a_role_that_is_not_scheme_managed_is_skipped_even_when_the_map_names_it() {
        let mut roles = [channel_role("custom_group_user", false)];
        apply_higher_scoped(&mut roles, &higher_scope_for("custom_group_user"));
        assert_eq!(
            roles[0].permissions,
            Some(vec!["create_post".to_owned(), "edit_post".to_owned()]),
            "SchemeManaged gates the merge; without it the row's list is final"
        );
    }

    /// A store pointed at a database that cannot be reached, so every call fails fast. The
    /// timeout is capped hard: sqlx's 30-second default made six tests in this workspace sit on
    /// their hands, and a suite that waits is a bug.
    fn app_with_an_unreachable_database() -> App {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nothing")
            .expect("a lazy pool needs no server");
        App::new(mm_store::SqlStore::from_pool(pool))
    }

    /// The three reads carry three different `where`/`id` pairs, and a broken database is a 500
    /// on all of them. The ids are what a client branches on, so they are pinned per function.
    #[tokio::test]
    async fn a_store_failure_is_a_500_with_each_functions_own_error_id() {
        let app = app_with_an_unreachable_database();

        let err = app.get_role("x").await.expect_err("no database, no role");
        assert_eq!(err.status_code, 500);
        assert_eq!(err.id, "app.role.get.app_error");
        assert_eq!(err.where_, "GetRole");

        let err = app
            .get_role_by_name("system_user")
            .await
            .expect_err("no database, no role");
        assert_eq!(err.status_code, 500);
        assert_eq!(err.id, "app.role.get_by_name.app_error");
        assert_eq!(err.where_, "GetRoleByName");

        let err = app
            .get_all_roles()
            .await
            .expect_err("no database, no roles");
        assert_eq!(err.status_code, 500);
        assert_eq!(err.id, "app.role.get_all.app_error");
        assert_eq!(err.where_, "GetAllRoles");
    }

    /// The merge short-circuits before touching the store when nothing in hand is scheme-managed
    /// — which is why this can assert `Ok` against a database that does not exist. Go's guard is
    /// `len(higherScopeNamesToQuery) == 0` (role.go:120); losing it would turn every read on a
    /// stock server into a second query, and here into a 500.
    #[tokio::test]
    async fn no_scheme_managed_role_means_no_second_query() {
        let app = app_with_an_unreachable_database();
        let mut roles = vec![
            Role {
                name: "custom_group_user".to_owned(),
                scheme_managed: false,
                permissions: Some(vec!["create_post".to_owned()]),
                ..Default::default()
            },
            Role {
                name: "system_post_all".to_owned(),
                scheme_managed: false,
                permissions: Some(vec![]),
                ..Default::default()
            },
        ];

        app.merge_channel_higher_scoped_permissions(&mut roles)
            .await
            .expect("the store is never reached");

        // And the permissions are untouched, not replaced by an empty merge result.
        assert_eq!(roles[0].permissions, Some(vec!["create_post".to_owned()]));
        assert_eq!(roles[1].permissions, Some(vec![]));
    }

    /// One scheme-managed role among many is enough to make the query happen — the guard is
    /// "none of them", not "the first one".
    #[tokio::test]
    async fn one_scheme_managed_role_is_enough_to_query() {
        let app = app_with_an_unreachable_database();
        let mut roles = vec![
            Role {
                name: "custom_group_user".to_owned(),
                scheme_managed: false,
                ..Default::default()
            },
            Role {
                name: "channel_user".to_owned(),
                scheme_managed: true,
                ..Default::default()
            },
        ];

        let err = app
            .merge_channel_higher_scoped_permissions(&mut roles)
            .await
            .expect_err("the unreachable store is reached");
        assert_eq!(err.status_code, 500);
        // Go names the *merge*, not the caller, and reuses the by-names id.
        assert_eq!(err.where_, "mergeChannelHigherScopedPermissions");
        assert_eq!(err.id, "app.role.get_by_names.app_error");
    }
}
