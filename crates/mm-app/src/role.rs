//! Port of `channels/app/role.go`: the reads — `GetRole` (:23), `GetAllRoles` (:42),
//! `Server.GetRoleByName` (:57) and the merge every one of them ends with (:107) — and, since
//! 2026-09-14, `PatchRole` (:146), `UpdateRole` (:188) and `sendUpdatedRoleEvent` (:279).
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

use mm_model::role::{
    BUILT_IN_SCHEME_MANAGED_ROLE_IDS, CHANNEL_ADMIN_ROLE_ID, CHANNEL_GUEST_ROLE_ID,
    CHANNEL_USER_ROLE_ID, NEW_SYSTEM_ROLE_IDS, Role, RolePatch,
};
use mm_model::scheme::{
    SCHEME_SCOPE_CHANNEL, SCHEME_SCOPE_PLAYBOOK, SCHEME_SCOPE_RUN, SCHEME_SCOPE_TEAM,
};
use mm_model::utils::{AppError, AppResult};
use mm_model::websocket_message::{WEBSOCKET_EVENT_ROLE_UPDATED, WebSocketEvent};
use mm_store::{ChannelStore, RoleStore, SchemeStore, StoreError, TeamStore};

use crate::App;

impl App {
    /// Port of `app.App.GetRole` (role.go:23) — `GET /api/v4/roles/{role_id}`.
    ///
    /// A missing row is Go's `store.ErrNotFound`, which the app layer turns into **404
    /// `app.role.get.app_error`** — the *same id* it uses for a database failure at 500. Only the
    /// status distinguishes them, so a port that invents a second id breaks any client branching
    /// on `id`.
    #[tracing::instrument(skip(self))]
    pub async fn get_role(&self, id: &str) -> AppResult<Role> {
        let role = self.store().role().get(id).await.map_err(|err| {
            tracing::error!(error = %err, "role lookup failed");
            AppError::boxed(
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
                return Err(AppError::boxed(
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
            AppError::boxed(
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
    pub async fn get_role_by_name(&self, name: &str) -> AppResult<Role> {
        let role = self.store().role().get_by_name(name).await.map_err(|err| {
            tracing::error!(error = %err, "role lookup by name failed");
            AppError::boxed(
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
                return Err(AppError::boxed(
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
            AppError::boxed(
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
    pub async fn get_all_roles(&self) -> AppResult<Vec<Role>> {
        let mut roles = self.store().role().get_all().await.map_err(|err| {
            tracing::error!(error = %err, "reading every role failed");
            AppError::boxed(
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
    async fn merge_channel_higher_scoped_permissions(&self, roles: &mut [Role]) -> AppResult<()> {
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
                AppError::boxed(
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

impl App {
    /// Port of `App.PatchRole` (role.go:146) — the write behind `PUT /roles/{id}/patch`.
    ///
    /// **The no-op shortcut is on the set, not the request.** `reflect.DeepEqual(*patch
    /// .Permissions, role.Permissions)` runs *after* the handler has sorted and de-duplicated
    /// `patch.Permissions`, and the stored column is itself sorted, so a patch naming the stored
    /// set in any order answers with the role exactly as read and touches nothing, while a patch
    /// naming a different set is a write — `update_at` stamped, the event sent. A `null` or
    /// absent `permissions` is **not** the shortcut: it falls through to `UpdateRole`, which
    /// rewrites the row and re-stamps `update_at` even though the permissions do not change.
    #[tracing::instrument(skip_all, fields(role_id = %role.id, no_op))]
    pub async fn patch_role(&self, mut role: Role, patch: &RolePatch) -> AppResult<Role> {
        if let Some(permissions) = &patch.permissions
            && role.permissions.as_ref() == Some(permissions)
        {
            tracing::Span::current().record("no_op", true);
            return Ok(role);
        }
        tracing::Span::current().record("no_op", false);

        role.patch(patch);
        let saved = self.update_role(&role).await?;
        self.send_updated_role_event(&saved).await?;
        Ok(saved)
    }

    /// Port of `App.UpdateRole` (role.go:188): the save, then the roles the change reaches.
    ///
    /// A built-in role that is not a channel role — and the four `NewSystemRoleIDs` — is
    /// inherited by nothing, so the saved row comes straight back. A built-in **channel** role
    /// is the default every channel scheme's roles merge their higher-scoped permissions from,
    /// so every live channel-scheme role is re-merged and announced; any other role is taken
    /// for a team-scheme default and the channel-scheme roles under it are. The announced roles
    /// are the impacted ones **other than** this one — this one's event belongs to the caller.
    ///
    /// Go appends the *argument* (the role as patched, before the store stamped it) to the
    /// impacted list and merges through that pointer, which is why what it returns is the
    /// store's copy rather than the merged one; the same two values are kept apart here.
    pub async fn update_role(&self, role: &Role) -> AppResult<Role> {
        let saved = self
            .store()
            .role()
            .save(role)
            .await
            .map_err(|err| match err {
                StoreError::InvalidInput { .. } => AppError::boxed(
                    "UpdateRole",
                    "app.role.save.invalid_role.app_error",
                    None,
                    String::new(),
                    400,
                ),
                other => {
                    tracing::error!(error = %other, "the role save failed");
                    AppError::boxed(
                        "UpdateRole",
                        "app.role.save.insert.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })?;

        let built_in_channel_roles = [
            CHANNEL_GUEST_ROLE_ID,
            CHANNEL_USER_ROLE_ID,
            CHANNEL_ADMIN_ROLE_ID,
        ];
        let inherited_by_nothing = BUILT_IN_SCHEME_MANAGED_ROLE_IDS
            .iter()
            .filter(|name| !built_in_channel_roles.contains(name))
            .chain(NEW_SYSTEM_ROLE_IDS.iter())
            .any(|name| *name == saved.name);
        if inherited_by_nothing {
            return Ok(saved);
        }

        let impacted = if built_in_channel_roles.contains(&saved.name.as_str()) {
            self.store().role().all_channel_scheme_roles().await
        } else {
            self.store()
                .role()
                .channel_roles_under_team_role(&saved.name)
                .await
        };
        let mut impacted = impacted.map_err(|err| {
            tracing::error!(error = %err, "the impacted roles could not be read");
            AppError::boxed(
                "UpdateRole",
                "app.role.get.app_error",
                None,
                String::new(),
                500,
            )
        })?;
        // A copy, as in Go: the merge rewrites this entry's permissions and the caller keeps its
        // own value.
        impacted.push(role.clone());
        self.merge_channel_higher_scoped_permissions(&mut impacted)
            .await?;

        for impacted_role in &impacted {
            if impacted_role.name != saved.name {
                self.send_updated_role_event(impacted_role).await?;
            }
        }

        Ok(saved)
    }

    /// Port of `App.sendUpdatedRoleEvent` (role.go:279): `role_updated`, the role as a **JSON
    /// string** under `role` — `json.Marshal`, HTML-escaped — to everyone for a built-in or
    /// scheme-less role, to each team of a team scheme, to each channel of a channel scheme,
    /// and to everyone again for a playbook or run scheme. A scheme that cannot be read is
    /// logged and skipped, not an error; a scope Go does not know is the 500.
    pub async fn send_updated_role_event(&self, role: &Role) -> AppResult<()> {
        let json = mm_model::utils::go_json_marshal(role).map_err(|err| {
            tracing::error!(error = %err, "failed to serialise the role for its event");
            AppError::boxed(
                "sendUpdatedRoleEvent",
                "api.marshal_error",
                None,
                String::new(),
                500,
            )
        })?;
        // One string per event: `message.Add` stores the value on each event Go builds.
        let event = |team_id: &str, channel_id: &str| {
            let mut message = WebSocketEvent::new(
                WEBSOCKET_EVENT_ROLE_UPDATED,
                team_id,
                channel_id,
                "",
                None,
                "",
            );
            message.add("role", serde_json::Value::String(json.clone()));
            message
        };

        // Built-in system roles apply to all users; broadcast globally without a DB lookup.
        if role.built_in {
            self.publish(event("", "")).await;
            return Ok(());
        }
        // No owning scheme — treat as global (e.g. custom non-scheme role).
        let Some(scheme_id) = role.scheme_id.as_deref() else {
            self.publish(event("", "")).await;
            return Ok(());
        };
        let scheme = match self.store().scheme().get(scheme_id).await {
            Ok(Some(scheme)) => scheme,
            Ok(None) => {
                tracing::error!(role_id = %role.id, scheme_id, "Failed to look up scheme for role event; skipping broadcast");
                return Ok(());
            }
            Err(err) => {
                tracing::error!(role_id = %role.id, scheme_id, error = %err, "Failed to look up scheme for role event; skipping broadcast");
                return Ok(());
            }
        };

        const PAGE_SIZE: usize = 1000;
        const MAX_BROADCASTS: usize = 100_000;
        let store_error = |err: StoreError| {
            tracing::error!(error = %err, "the scheme's holders could not be read");
            AppError::boxed(
                "sendUpdatedRoleEvent",
                "app.role.send_updated_role_event.app_error",
                None,
                String::new(),
                500,
            )
        };
        match scheme.scope.as_str() {
            SCHEME_SCOPE_TEAM => {
                let mut total = 0;
                let mut offset = 0;
                loop {
                    let teams = self
                        .store()
                        .team()
                        .get_teams_by_scheme(&scheme.id, offset, PAGE_SIZE as i64)
                        .await
                        .map_err(store_error)?;
                    for team in &teams {
                        self.publish(event(&team.id, "")).await;
                    }
                    total += teams.len();
                    if teams.len() < PAGE_SIZE {
                        break;
                    }
                    if total >= MAX_BROADCASTS {
                        tracing::error!(scheme_id = %scheme.id, total, "sendUpdatedRoleEvent: hit broadcast limit for team scheme");
                        break;
                    }
                    offset += PAGE_SIZE as i64;
                }
            }
            SCHEME_SCOPE_CHANNEL => {
                let mut total = 0;
                let mut offset = 0;
                loop {
                    let channels = self
                        .store()
                        .channel()
                        .get_channels_by_scheme(&scheme.id, offset, PAGE_SIZE as i64)
                        .await
                        .map_err(store_error)?;
                    for channel in &channels {
                        self.publish(event("", &channel.id)).await;
                    }
                    total += channels.len();
                    if channels.len() < PAGE_SIZE {
                        break;
                    }
                    if total >= MAX_BROADCASTS {
                        tracing::error!(scheme_id = %scheme.id, total, "sendUpdatedRoleEvent: hit broadcast limit for channel scheme");
                        break;
                    }
                    offset += PAGE_SIZE as i64;
                }
            }
            // Playbook/run schemes don't map to teams or channels; broadcast globally.
            SCHEME_SCOPE_PLAYBOOK | SCHEME_SCOPE_RUN => self.publish(event("", "")).await,
            other => {
                return Err(AppError::boxed(
                    "sendUpdatedRoleEvent",
                    "app.role.send_updated_role_event.unknown_scope",
                    None,
                    format!("unknown scheme scope: {other}"),
                    500,
                ));
            }
        }
        Ok(())
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
