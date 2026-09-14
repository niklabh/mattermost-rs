//! Port of `SqlRoleStore` (channels/store/sqlstore/role_store.go) — the four read paths, the
//! save, and the two channel-scheme reads `UpdateRole` needs.
//!
//! # `Permissions` is one text column, not a list
//!
//! The `Roles.Permissions` column holds every permission the role has as a **single
//! space-separated string**, and Go writes it with a leading space per entry
//! (`fmt.Sprintf(" %v", permission)`, role_store.go:52) so the stored value starts with one. It
//! reads back through `strings.Fields`, which collapses any run of whitespace and drops the
//! leading one, so the write and read shapes are not symmetric and the column's exact spacing is
//! not load-bearing on the way in.
//!
//! That matters for the port in one specific way: `strings.Fields("")` returns an **empty
//! non-nil** slice, so a role with no permissions comes back as `[]` rather than `null` on the
//! wire. Every row read here therefore carries `Some(vec![])` at minimum — never `None`.
//!
//! # Neither `Get` nor `GetAll` filters `DeleteAt`
//!
//! A soft-deleted role is returned by all four of these. That is Go's behaviour and it is load
//! bearing: `Delete` only stamps `DeleteAt`, and permission checks still need to resolve the role
//! a member's `Roles` column names.

use std::collections::BTreeMap;

use mm_model::role::{
    CHANNEL_ADMIN_ROLE_ID, CHANNEL_GUEST_ROLE_ID, CHANNEL_USER_ROLE_ID, Role, RolePermissions,
};
use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `store.RoleStore` that is ported.
pub trait RoleStore {
    /// Port of `SqlRoleStore.Get` (role_store.go:221).
    fn get(
        &self,
        role_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<Role>, StoreError>> + Send;

    /// Port of `SqlRoleStore.GetAll` (role_store.go:235).
    fn get_all(&self) -> impl std::future::Future<Output = Result<Vec<Role>, StoreError>> + Send;

    /// Port of `SqlRoleStore.GetByName` (role_store.go:250).
    fn get_by_name(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Result<Option<Role>, StoreError>> + Send;

    /// Port of `SqlRoleStore.GetByNames` (role_store.go:264).
    fn get_by_names(
        &self,
        names: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<Role>, StoreError>> + Send;

    /// Port of `SqlRoleStore.ChannelHigherScopedPermissions` (role_store.go:419).
    fn channel_higher_scoped_permissions(
        &self,
        role_names: &[String],
    ) -> impl std::future::Future<Output = Result<BTreeMap<String, RolePermissions>, StoreError>> + Send;

    /// Port of `SqlRoleStore.Save` (role_store.go:103) — an insert when `id` is empty, an
    /// update of every column otherwise.
    ///
    /// `IsValidWithoutId` first, as [`StoreError::InvalidInput`], which the app layer answers
    /// **400**; anything else is its 500. The returned role is Go's `dbRole.ToModel()`: the
    /// input with `update_at` (and, on insert, `id` and `create_at`) stamped, and `permissions`
    /// as the column reads back — de-duplicated in first-occurrence order by
    /// `NewRoleFromModel`'s map, never `None`. It is **not** re-read from the table.
    fn save(
        &self,
        role: &Role,
    ) -> impl std::future::Future<Output = Result<Role, StoreError>> + Send;

    /// Port of `SqlRoleStore.AllChannelSchemeRoles` (role_store.go:438): every live role of
    /// every live channel-scoped scheme.
    fn all_channel_scheme_roles(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<Role>, StoreError>> + Send;

    /// Port of `SqlRoleStore.ChannelRolesUnderTeamRole` (role_store.go:478): the channel-scheme
    /// roles of every channel whose team's scheme names `role_name` as a default channel role.
    fn channel_roles_under_team_role(
        &self,
        role_name: &str,
    ) -> impl std::future::Future<Output = Result<Vec<Role>, StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlRoleStore {
    pool: PgPool,
}

impl SqlRoleStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// Port of the unexported `sqlstore.Role` (role_store.go:26) — the row shape, which differs from
/// `model.Role` in exactly one field.
struct RoleRow {
    id: String,
    name: Option<String>,
    display_name: Option<String>,
    description: Option<String>,
    create_at: Option<i64>,
    update_at: Option<i64>,
    delete_at: Option<i64>,
    permissions: Option<String>,
    scheme_managed: Option<bool>,
    built_in: Option<bool>,
    scheme_id: Option<String>,
}

impl RoleRow {
    /// Port of `(Role).ToModel` (role_store.go:75).
    ///
    /// Every column except `Id` is nullable in the schema the Go migrations create, and Go scans
    /// them through types whose zero value is what a NULL becomes. `unwrap_or_default` is that
    /// same rule rather than a looser one.
    fn into_model(self) -> Role {
        Role {
            id: self.id,
            name: self.name.unwrap_or_default(),
            display_name: self.display_name.unwrap_or_default(),
            description: self.description.unwrap_or_default(),
            create_at: self.create_at.unwrap_or_default(),
            update_at: self.update_at.unwrap_or_default(),
            delete_at: self.delete_at.unwrap_or_default(),
            // `strings.Fields`: split on any whitespace run, dropping empties. Never `None` —
            // Go's Fields returns an empty slice for an empty column, not nil.
            permissions: Some(split_permissions(
                self.permissions.as_deref().unwrap_or_default(),
            )),
            scheme_managed: self.scheme_managed.unwrap_or_default(),
            built_in: self.built_in.unwrap_or_default(),
            scheme_id: self.scheme_id,
        }
    }
}

/// Port of `strings.Fields` as role_store.go applies it to the `Permissions` column.
///
/// `split_whitespace` is the same rule: any run of Unicode whitespace separates, and no empty
/// fields are produced, so the leading space Go's writer emits disappears.
fn split_permissions(permissions: &str) -> Vec<String> {
    permissions.split_whitespace().map(str::to_owned).collect()
}

/// Port of `NewRoleFromModel`'s column writer (role_store.go:50-56): every permission prefixed
/// with one space, **each kept once** — a `map[string]bool` drops repeats in first-occurrence
/// order — so the column starts with a space and the leading one is what `Fields` drops on the
/// way back.
fn join_permissions(permissions: &[String]) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut joined = String::new();
    for permission in permissions {
        if seen.insert(permission.as_str()) {
            joined.push(' ');
            joined.push_str(permission);
        }
    }
    joined
}

impl RoleStore for SqlRoleStore {
    #[tracing::instrument(skip(self))]
    async fn get(&self, role_id: &str) -> Result<Option<Role>, StoreError> {
        // Go returns `store.NewErrNotFound("Role", roleID)` for `sql.ErrNoRows`; a missing row is
        // an ordinary outcome here rather than an error, and the caller decides the status code.
        let row = sqlx::query_as!(
            RoleRow,
            r#"
            SELECT id,
                   name,
                   displayname   AS display_name,
                   description,
                   createat      AS create_at,
                   updateat      AS update_at,
                   deleteat      AS delete_at,
                   permissions,
                   schememanaged AS scheme_managed,
                   builtin       AS built_in,
                   schemeid      AS scheme_id
              FROM roles
             WHERE id = $1
            "#,
            role_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get Role with id={role_id}"),
            source,
        })?;

        Ok(row.map(RoleRow::into_model))
    }

    #[tracing::instrument(skip(self))]
    async fn get_all(&self) -> Result<Vec<Role>, StoreError> {
        // No WHERE clause at all in Go — deleted roles included. See the module note.
        let rows = sqlx::query_as!(
            RoleRow,
            r#"
            SELECT id,
                   name,
                   displayname   AS display_name,
                   description,
                   createat      AS create_at,
                   updateat      AS update_at,
                   deleteat      AS delete_at,
                   permissions,
                   schememanaged AS scheme_managed,
                   builtin       AS built_in,
                   schemeid      AS scheme_id
              FROM roles
            "#
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Roles".to_owned(),
            source,
        })?;

        Ok(rows.into_iter().map(RoleRow::into_model).collect())
    }

    #[tracing::instrument(skip(self))]
    async fn get_by_name(&self, name: &str) -> Result<Option<Role>, StoreError> {
        let row = sqlx::query_as!(
            RoleRow,
            r#"
            SELECT id,
                   name,
                   displayname   AS display_name,
                   description,
                   createat      AS create_at,
                   updateat      AS update_at,
                   deleteat      AS delete_at,
                   permissions,
                   schememanaged AS scheme_managed,
                   builtin       AS built_in,
                   schemeid      AS scheme_id
              FROM roles
             WHERE name = $1
            "#,
            name
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find Roles with name={name}"),
            source,
        })?;

        Ok(row.map(RoleRow::into_model))
    }

    #[tracing::instrument(skip(self))]
    async fn get_by_names(&self, names: &[String]) -> Result<Vec<Role>, StoreError> {
        // Go short-circuits before touching the database (role_store.go:265). Reproduced because
        // `WHERE name = ANY('{}')` is a round trip that always returns nothing, and because an
        // empty `IN ()` is a SQL error in the builder Go would otherwise produce.
        if names.is_empty() {
            return Ok(Vec::new());
        }

        let rows = sqlx::query_as!(
            RoleRow,
            r#"
            SELECT id,
                   name,
                   displayname   AS display_name,
                   description,
                   createat      AS create_at,
                   updateat      AS update_at,
                   deleteat      AS delete_at,
                   permissions,
                   schememanaged AS scheme_managed,
                   builtin       AS built_in,
                   schemeid      AS scheme_id
              FROM roles
             WHERE name = ANY($1)
            "#,
            names
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Roles".to_owned(),
            source,
        })?;

        Ok(rows.into_iter().map(RoleRow::into_model).collect())
    }

    #[tracing::instrument(skip(self))]
    async fn channel_higher_scoped_permissions(
        &self,
        role_names: &[String],
    ) -> Result<BTreeMap<String, RolePermissions>, StoreError> {
        self.channel_higher_scoped_permissions_impl(role_names)
            .await
    }

    #[tracing::instrument(skip(self, role), fields(role_id = %role.id, role_name = %role.name))]
    async fn save(&self, role: &Role) -> Result<Role, StoreError> {
        role.is_valid_without_id()
            .map_err(|err| StoreError::InvalidInput {
                entity: "Role",
                field: "<any>",
                value: err.to_string(),
            })?;

        let permissions = join_permissions(role.permissions.as_deref().unwrap_or_default());
        let now = mm_model::utils::get_millis();
        // `ToModel` on the row that was written: the caller's fields, the stamps, and the
        // permissions as `strings.Fields` reads the column back.
        let saved = |id: String, create_at: i64| Role {
            id,
            name: role.name.clone(),
            display_name: role.display_name.clone(),
            description: role.description.clone(),
            create_at,
            update_at: now,
            delete_at: role.delete_at,
            permissions: Some(split_permissions(&permissions)),
            scheme_managed: role.scheme_managed,
            built_in: role.built_in,
            scheme_id: role.scheme_id.clone(),
        };

        if role.id.is_empty() {
            let id = mm_model::utils::new_id();
            sqlx::query!(
                r#"
                INSERT INTO roles
                    (id, name, displayname, description, permissions, createat, updateat,
                     deleteat, schememanaged, builtin, schemeid)
                VALUES ($1, $2, $3, $4, $5, $6, $6, $7, $8, $9, $10)
                "#,
                id,
                role.name,
                role.display_name,
                role.description,
                permissions,
                now,
                role.delete_at,
                role.scheme_managed,
                role.built_in,
                role.scheme_id.as_deref(),
            )
            .execute(&self.pool)
            .await
            .map_err(|source| StoreError::Db {
                context: "failed to save Role".to_owned(),
                source,
            })?;
            return Ok(saved(id, now));
        }

        let result = sqlx::query!(
            r#"
            UPDATE roles
               SET updateat = $2, deleteat = $3, createat = $4, name = $5, displayname = $6,
                   description = $7, permissions = $8, schememanaged = $9, builtin = $10,
                   schemeid = $11
             WHERE id = $1
            "#,
            role.id,
            now,
            role.delete_at,
            role.create_at,
            role.name,
            role.display_name,
            role.description,
            permissions,
            role.scheme_managed,
            role.built_in,
            role.scheme_id.as_deref(),
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to update Role".to_owned(),
            source,
        })?;
        if result.rows_affected() != 1 {
            // `fmt.Errorf("invalid number of updated rows, expected 1 but got %d", …)` — a plain
            // error in Go, so the app layer's 500 rather than a not-found.
            return Err(StoreError::Stale {
                entity: "Role",
                detail: "invalid number of updated rows, expected 1",
            });
        }
        Ok(saved(role.id.clone(), role.create_at))
    }

    #[tracing::instrument(skip(self))]
    async fn all_channel_scheme_roles(&self) -> Result<Vec<Role>, StoreError> {
        let rows = sqlx::query_as!(
            RoleRow,
            r#"
            SELECT roles.id,
                   roles.name,
                   roles.displayname   AS display_name,
                   roles.description,
                   roles.createat      AS create_at,
                   roles.updateat      AS update_at,
                   roles.deleteat      AS delete_at,
                   roles.permissions,
                   roles.schememanaged AS scheme_managed,
                   roles.builtin       AS built_in,
                   roles.schemeid      AS scheme_id
              FROM roles
              JOIN schemes ON roles.schemeid = schemes.id
             WHERE schemes.scope = 'channel'
               AND roles.deleteat = 0
               AND schemes.deleteat = 0
            "#
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Roles".to_owned(),
            source,
        })?;
        Ok(rows.into_iter().map(RoleRow::into_model).collect())
    }

    #[tracing::instrument(skip(self))]
    async fn channel_roles_under_team_role(
        &self,
        role_name: &str,
    ) -> Result<Vec<Role>, StoreError> {
        let rows = sqlx::query_as!(
            RoleRow,
            r#"
            SELECT channelschemeroles.id,
                   channelschemeroles.name,
                   channelschemeroles.displayname   AS display_name,
                   channelschemeroles.description,
                   channelschemeroles.createat      AS create_at,
                   channelschemeroles.updateat      AS update_at,
                   channelschemeroles.deleteat      AS delete_at,
                   channelschemeroles.permissions,
                   channelschemeroles.schememanaged AS scheme_managed,
                   channelschemeroles.builtin       AS built_in,
                   channelschemeroles.schemeid      AS scheme_id
              FROM roles AS higherscopedroles
              JOIN schemes AS higherscopedschemes
                ON (higherscopedroles.name = higherscopedschemes.defaultchannelguestrole
                    OR higherscopedroles.name = higherscopedschemes.defaultchanneluserrole
                    OR higherscopedroles.name = higherscopedschemes.defaultchanneladminrole)
              JOIN teams ON teams.schemeid = higherscopedschemes.id
              JOIN channels ON channels.teamid = teams.id
              JOIN schemes AS channelschemes ON channels.schemeid = channelschemes.id
              JOIN roles AS channelschemeroles ON channelschemeroles.schemeid = channelschemes.id
             WHERE higherscopedschemes.scope = 'team'
               AND higherscopedroles.name = $1
               AND higherscopedroles.deleteat = 0
               AND higherscopedschemes.deleteat = 0
               AND teams.deleteat = 0
               AND channels.deleteat = 0
               AND channelschemes.deleteat = 0
               AND channelschemeroles.deleteat = 0
            "#,
            role_name
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Roles".to_owned(),
            source,
        })?;
        Ok(rows.into_iter().map(RoleRow::into_model).collect())
    }
}

/// Port of the unexported `channelRolesPermissions` (role_store.go:40) — one UNION row.
struct ChannelRolesPermissionsRow {
    guest_role_name: Option<String>,
    user_role_name: Option<String>,
    admin_role_name: Option<String>,
    higher_scoped_guest_permissions: Option<String>,
    higher_scoped_user_permissions: Option<String>,
    higher_scoped_admin_permissions: Option<String>,
}

/// Port of `strings.Split(s, " ")` as `ChannelHigherScopedPermissions` applies it (role_store.go:432).
///
/// **This is not the same function the row mapper uses.** `ToModel` splits the identical column with
/// `strings.Fields`; this one splits on a single space with no collapsing, so the leading space Go's
/// writer emits survives as an **empty first element**, and an empty column becomes `[""]` — a
/// one-element slice, not an empty one.
///
/// Reproduced rather than corrected. It is harmless downstream — `MergeChannelHigherScopedPermissions`
/// only ever asks the list for membership, and `""` is not a permission id — but "harmless" is a
/// property of today's only caller, not of the function, and the two splitters disagreeing is the
/// kind of difference a port silently irons out. See [D-132].
fn split_on_single_space(permissions: &str) -> Vec<String> {
    permissions.split(' ').map(str::to_owned).collect()
}

impl SqlRoleStore {
    /// Port of `channelHigherScopedPermissionsQuery` (role_store.go:345) and its caller.
    ///
    /// Three UNIONed branches, and the third is the one that fires on Team Edition:
    ///
    /// 1. A channel scheme's **user** and **admin** roles, whose higher scope is the *team*
    ///    scheme's channel roles.
    /// 2. The same for the **guest** role.
    /// 3. A channel scheme whose team has **no scheme**, whose higher scope is therefore the
    ///    system default — and since no system scheme record ships with Mattermost, those three
    ///    roles are matched **by name** rather than by column. Go's comment at :407 says exactly
    ///    this.
    ///
    /// # The `IN` list is parameterised here and interpolated in Go
    ///
    /// Go builds the list with `strings.Join(roleNames, "', '")` straight into the SQL text
    /// (:411). A role name containing an apostrophe would break the statement or inject into it.
    /// `IsValidRoleName` restricts names to `[a-z0-9_]` on the way in, so the hole is not reachable
    /// through the API today — but it is reachable through any row written by something that did
    /// not validate, and reproducing a string-interpolated query in order to be faithful would be
    /// choosing bug-compatibility over the one thing the two servers must never disagree about.
    /// This binds `$1` instead. For every legal role name the results are identical. [D-133].
    async fn channel_higher_scoped_permissions_impl(
        &self,
        role_names: &[String],
    ) -> Result<BTreeMap<String, RolePermissions>, StoreError> {
        let rows = sqlx::query_as!(
            ChannelRolesPermissionsRow,
            r#"
            SELECT CAST('' AS varchar)               AS guest_role_name,
                   roleschemes.defaultchanneluserrole  AS user_role_name,
                   roleschemes.defaultchanneladminrole AS admin_role_name,
                   CAST('' AS text)                  AS higher_scoped_guest_permissions,
                   userroles.permissions             AS higher_scoped_user_permissions,
                   adminroles.permissions            AS higher_scoped_admin_permissions
              FROM schemes AS roleschemes
              JOIN channels ON channels.schemeid = roleschemes.id
              JOIN teams ON teams.id = channels.teamid
              JOIN schemes ON schemes.id = teams.schemeid
             RIGHT JOIN roles AS userroles ON userroles.name = schemes.defaultchanneluserrole
             RIGHT JOIN roles AS adminroles ON adminroles.name = schemes.defaultchanneladminrole
             WHERE roleschemes.defaultchanneluserrole = ANY($1)
                OR roleschemes.defaultchanneladminrole = ANY($1)

            UNION

            SELECT roleschemes.defaultchannelguestrole AS guest_role_name,
                   CAST('' AS varchar)                 AS user_role_name,
                   CAST('' AS varchar)                 AS admin_role_name,
                   guestroles.permissions              AS higher_scoped_guest_permissions,
                   CAST('' AS text)                    AS higher_scoped_user_permissions,
                   CAST('' AS text)                    AS higher_scoped_admin_permissions
              FROM schemes AS roleschemes
              JOIN channels ON channels.schemeid = roleschemes.id
              JOIN teams ON teams.id = channels.teamid
              JOIN schemes ON schemes.id = teams.schemeid
             RIGHT JOIN roles AS guestroles ON guestroles.name = schemes.defaultchannelguestrole
             WHERE roleschemes.defaultchannelguestrole = ANY($1)

            UNION

            SELECT schemes.defaultchannelguestrole AS guest_role_name,
                   schemes.defaultchanneluserrole  AS user_role_name,
                   schemes.defaultchanneladminrole AS admin_role_name,
                   guestroles.permissions          AS higher_scoped_guest_permissions,
                   userroles.permissions           AS higher_scoped_user_permissions,
                   adminroles.permissions          AS higher_scoped_admin_permissions
              FROM schemes
              JOIN channels ON channels.schemeid = schemes.id
              JOIN teams ON teams.id = channels.teamid
              JOIN roles AS guestroles ON guestroles.name = $2
              JOIN roles AS userroles ON userroles.name = $3
              JOIN roles AS adminroles ON adminroles.name = $4
             WHERE (schemes.defaultchannelguestrole = ANY($1)
                 OR schemes.defaultchanneluserrole = ANY($1)
                 OR schemes.defaultchanneladminrole = ANY($1))
               AND (teams.schemeid = '' OR teams.schemeid IS NULL)
            "#,
            role_names,
            CHANNEL_GUEST_ROLE_ID,
            CHANNEL_USER_ROLE_ID,
            CHANNEL_ADMIN_ROLE_ID,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find RolePermissions".to_owned(),
            source,
        })?;

        // Go writes all three keys for every row, unconditionally (:431-434) — including the `''`
        // names the first two branches select as literals. So the map gains an empty-string key
        // whose value is whichever row was processed last. Reproduced; see [D-132].
        let mut out: BTreeMap<String, RolePermissions> = BTreeMap::new();
        for row in rows {
            out.insert(
                row.guest_role_name.unwrap_or_default(),
                RolePermissions {
                    role_id: CHANNEL_GUEST_ROLE_ID.to_owned(),
                    permissions: split_on_single_space(
                        &row.higher_scoped_guest_permissions.unwrap_or_default(),
                    ),
                },
            );
            out.insert(
                row.user_role_name.unwrap_or_default(),
                RolePermissions {
                    role_id: CHANNEL_USER_ROLE_ID.to_owned(),
                    permissions: split_on_single_space(
                        &row.higher_scoped_user_permissions.unwrap_or_default(),
                    ),
                },
            );
            out.insert(
                row.admin_role_name.unwrap_or_default(),
                RolePermissions {
                    role_id: CHANNEL_ADMIN_ROLE_ID.to_owned(),
                    permissions: split_on_single_space(
                        &row.higher_scoped_admin_permissions.unwrap_or_default(),
                    ),
                },
            );
        }

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The column-to-list conversion, which is the only transformation this store performs.
    #[test]
    fn permissions_split_the_way_go_fields_does() {
        // Go's writer emits a leading space per entry, so this is the shape actually stored.
        assert_eq!(
            split_permissions(" create_post edit_post"),
            vec!["create_post".to_owned(), "edit_post".to_owned()]
        );
        // Any run of whitespace separates, and no empty fields survive.
        assert_eq!(
            split_permissions("  create_post \t\n edit_post  "),
            vec!["create_post".to_owned(), "edit_post".to_owned()]
        );
        // An empty column is an empty list, NOT a missing one: `strings.Fields("")` is `[]string{}`.
        assert!(split_permissions("").is_empty());
        assert!(split_permissions("   ").is_empty());
        // Duplicates survive a read; only the writer deduplicates (role_store.go:50).
        assert_eq!(
            split_permissions("create_post create_post"),
            vec!["create_post".to_owned(), "create_post".to_owned()]
        );
    }

    /// A row with no permissions must serialise as `[]`, never `null` — the distinction
    /// `mm-model`'s `Option<Vec<String>>` exists to carry, and the reason `to_model` always wraps
    /// in `Some`.
    #[test]
    fn a_role_with_no_permissions_reads_as_an_empty_list() {
        let row = RoleRow {
            id: "role1jbyqbtxbtqcgy3wa9tjh".to_owned(),
            name: Some("custom_role".to_owned()),
            display_name: None,
            description: None,
            create_at: None,
            update_at: None,
            delete_at: None,
            permissions: None,
            scheme_managed: None,
            built_in: None,
            scheme_id: None,
        };
        let role = row.into_model();

        assert_eq!(role.permissions, Some(Vec::new()));
        assert_eq!(
            serde_json::to_value(&role).expect("serialises")["permissions"],
            serde_json::Value::Array(vec![])
        );
        // Every NULL column becomes its zero value, exactly as Go's scan targets do.
        assert_eq!(role.display_name, "");
        assert_eq!(role.create_at, 0);
        assert!(!role.built_in);
        // `SchemeId` is the one genuinely optional field: `*string` in Go, `null` on the wire.
        assert_eq!(role.scheme_id, None);
        assert_eq!(
            serde_json::to_value(&role).expect("serialises")["scheme_id"],
            serde_json::Value::Null
        );
    }
}
