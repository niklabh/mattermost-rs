//! Port of `SqlSchemeStore` (channels/store/sqlstore/scheme_store.go) — the four read paths.
//!
//! # `Schemes` is empty on Team Edition
//!
//! Schemes are an enterprise feature, so the development database has zero rows and every team and
//! channel resolves its roles from the built-in defaults instead. That is why `team_store.rs`
//! LEFT-joins this table and treats every column as nullable, and it is why the DB-backed tests
//! here insert their own rows rather than reading what the Go server left behind: unlike `Roles`,
//! there is nothing to read.
//!
//! # The three read paths disagree about deleted schemes
//!
//! `Get` and `GetByName` return a soft-deleted scheme; `GetAllPage` and `CountByScope` filter
//! `DeleteAt = 0`. Reproduced rather than harmonised — a lookup by id has to resolve the scheme a
//! stale row still points at, while a listing should not offer it.

use mm_model::scheme::Scheme;
use sqlx::PgPool;

use crate::error::StoreError;

/// The read subset of Go's `store.SchemeStore` that is ported.
pub trait SchemeStore {
    /// Port of `SqlSchemeStore.Get` (scheme_store.go:327).
    fn get(
        &self,
        scheme_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<Scheme>, StoreError>> + Send;

    /// Port of `SqlSchemeStore.GetByName` (scheme_store.go:339).
    fn get_by_name(
        &self,
        scheme_name: &str,
    ) -> impl std::future::Future<Output = Result<Option<Scheme>, StoreError>> + Send;

    /// Port of `SqlSchemeStore.GetAllPage` (scheme_store.go:417).
    fn get_all_page(
        &self,
        scope: &str,
        offset: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<Scheme>, StoreError>> + Send;

    /// Port of `SqlSchemeStore.CountByScope` (scheme_store.go:471).
    fn count_by_scope(
        &self,
        scope: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlSchemeStore {
    pool: PgPool,
}

impl SqlSchemeStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// The row shape. Unlike `Roles`, every column maps straight onto the model — Go scans directly
/// into `model.Scheme` here (scheme_store.go:328) with no intermediate type.
struct SchemeRow {
    id: String,
    name: Option<String>,
    display_name: Option<String>,
    description: Option<String>,
    create_at: Option<i64>,
    update_at: Option<i64>,
    delete_at: Option<i64>,
    scope: Option<String>,
    default_team_admin_role: Option<String>,
    default_team_user_role: Option<String>,
    default_team_guest_role: Option<String>,
    default_channel_admin_role: Option<String>,
    default_channel_user_role: Option<String>,
    default_channel_guest_role: Option<String>,
    default_playbook_admin_role: Option<String>,
    default_playbook_member_role: Option<String>,
    default_run_admin_role: Option<String>,
    default_run_member_role: Option<String>,
}

impl SchemeRow {
    /// Every column but `Id` is nullable in the schema Go's migrations create, and `model.Scheme`
    /// holds plain strings, so a NULL is the empty string — which is also what the four
    /// playbook/run columns default to at the database level.
    fn into_model(self) -> Scheme {
        Scheme {
            id: self.id,
            name: self.name.unwrap_or_default(),
            display_name: self.display_name.unwrap_or_default(),
            description: self.description.unwrap_or_default(),
            create_at: self.create_at.unwrap_or_default(),
            update_at: self.update_at.unwrap_or_default(),
            delete_at: self.delete_at.unwrap_or_default(),
            scope: self.scope.unwrap_or_default(),
            default_team_admin_role: self.default_team_admin_role.unwrap_or_default(),
            default_team_user_role: self.default_team_user_role.unwrap_or_default(),
            default_team_guest_role: self.default_team_guest_role.unwrap_or_default(),
            default_channel_admin_role: self.default_channel_admin_role.unwrap_or_default(),
            default_channel_user_role: self.default_channel_user_role.unwrap_or_default(),
            default_channel_guest_role: self.default_channel_guest_role.unwrap_or_default(),
            default_playbook_admin_role: self.default_playbook_admin_role.unwrap_or_default(),
            default_playbook_member_role: self.default_playbook_member_role.unwrap_or_default(),
            default_run_admin_role: self.default_run_admin_role.unwrap_or_default(),
            default_run_member_role: self.default_run_member_role.unwrap_or_default(),
        }
    }
}

impl SchemeStore for SqlSchemeStore {
    #[tracing::instrument(skip(self))]
    async fn get(&self, scheme_id: &str) -> Result<Option<Scheme>, StoreError> {
        let row = sqlx::query_as!(
            SchemeRow,
            r#"
            SELECT id,
                   name,
                   displayname               AS display_name,
                   description,
                   createat                  AS create_at,
                   updateat                  AS update_at,
                   deleteat                  AS delete_at,
                   scope,
                   defaultteamadminrole      AS default_team_admin_role,
                   defaultteamuserrole       AS default_team_user_role,
                   defaultteamguestrole      AS default_team_guest_role,
                   defaultchanneladminrole   AS default_channel_admin_role,
                   defaultchanneluserrole    AS default_channel_user_role,
                   defaultchannelguestrole   AS default_channel_guest_role,
                   defaultplaybookadminrole  AS default_playbook_admin_role,
                   defaultplaybookmemberrole AS default_playbook_member_role,
                   defaultrunadminrole       AS default_run_admin_role,
                   defaultrunmemberrole      AS default_run_member_role
              FROM schemes
             WHERE id = $1
            "#,
            scheme_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get Scheme with schemeId={scheme_id}"),
            source,
        })?;

        Ok(row.map(SchemeRow::into_model))
    }

    #[tracing::instrument(skip(self))]
    async fn get_by_name(&self, scheme_name: &str) -> Result<Option<Scheme>, StoreError> {
        let row = sqlx::query_as!(
            SchemeRow,
            r#"
            SELECT id,
                   name,
                   displayname               AS display_name,
                   description,
                   createat                  AS create_at,
                   updateat                  AS update_at,
                   deleteat                  AS delete_at,
                   scope,
                   defaultteamadminrole      AS default_team_admin_role,
                   defaultteamuserrole       AS default_team_user_role,
                   defaultteamguestrole      AS default_team_guest_role,
                   defaultchanneladminrole   AS default_channel_admin_role,
                   defaultchanneluserrole    AS default_channel_user_role,
                   defaultchannelguestrole   AS default_channel_guest_role,
                   defaultplaybookadminrole  AS default_playbook_admin_role,
                   defaultplaybookmemberrole AS default_playbook_member_role,
                   defaultrunadminrole       AS default_run_admin_role,
                   defaultrunmemberrole      AS default_run_member_role
              FROM schemes
             WHERE name = $1
            "#,
            scheme_name
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get Scheme with schemeName={scheme_name}"),
            source,
        })?;

        Ok(row.map(SchemeRow::into_model))
    }

    /// An empty `scope` means "every scope" — Go adds the predicate only when the argument is
    /// non-empty (scheme_store.go:449), so `""` is a wildcard rather than a filter that matches
    /// nothing. Expressed as one static statement to keep sqlx's compile-time checking, the same
    /// choice `team_store.rs` makes for its optional predicates.
    #[tracing::instrument(skip(self))]
    async fn get_all_page(
        &self,
        scope: &str,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<Scheme>, StoreError> {
        let rows = sqlx::query_as!(
            SchemeRow,
            r#"
            SELECT id,
                   name,
                   displayname               AS display_name,
                   description,
                   createat                  AS create_at,
                   updateat                  AS update_at,
                   deleteat                  AS delete_at,
                   scope,
                   defaultteamadminrole      AS default_team_admin_role,
                   defaultteamuserrole       AS default_team_user_role,
                   defaultteamguestrole      AS default_team_guest_role,
                   defaultchanneladminrole   AS default_channel_admin_role,
                   defaultchanneluserrole    AS default_channel_user_role,
                   defaultchannelguestrole   AS default_channel_guest_role,
                   defaultplaybookadminrole  AS default_playbook_admin_role,
                   defaultplaybookmemberrole AS default_playbook_member_role,
                   defaultrunadminrole       AS default_run_admin_role,
                   defaultrunmemberrole      AS default_run_member_role
              FROM schemes
             WHERE deleteat = 0
               AND ($1 = '' OR scope = $1)
             ORDER BY createat DESC
             LIMIT $2 OFFSET $3
            "#,
            scope,
            limit,
            offset
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to get Schemes".to_owned(),
            source,
        })?;

        Ok(rows.into_iter().map(SchemeRow::into_model).collect())
    }

    /// Unlike [`SchemeStore::get_all_page`], the scope here is **not** optional: Go's SQL has a
    /// bare `WHERE Scope = ?`, so an empty scope counts the schemes whose scope is the empty
    /// string — which is none of them — rather than counting everything.
    #[tracing::instrument(skip(self))]
    async fn count_by_scope(&self, scope: &str) -> Result<i64, StoreError> {
        let count = sqlx::query_scalar!(
            r#"SELECT count(*) AS "count!" FROM schemes WHERE scope = $1 AND deleteat = 0"#,
            scope
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to count Schemes by scope".to_owned(),
            source,
        })?;

        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_columns_become_empty_strings() {
        let row = SchemeRow {
            id: "schm1jbyqbtxbtqcgy3wa9tjh".to_owned(),
            name: Some("custom_scheme".to_owned()),
            display_name: None,
            description: None,
            create_at: None,
            update_at: None,
            delete_at: None,
            scope: None,
            default_team_admin_role: None,
            default_team_user_role: None,
            default_team_guest_role: None,
            default_channel_admin_role: None,
            default_channel_user_role: None,
            default_channel_guest_role: None,
            default_playbook_admin_role: None,
            default_playbook_member_role: None,
            default_run_admin_role: None,
            default_run_member_role: None,
        };
        let scheme = row.into_model();

        assert_eq!(scheme.name, "custom_scheme");
        assert_eq!(scheme.scope, "");
        assert_eq!(scheme.default_channel_admin_role, "");
        assert_eq!(scheme.create_at, 0);
        // A row of NULLs is not a valid scheme, which is the point: reads do not validate, so the
        // caller has to. `IsValidForCreate` is where that happens in Go too.
        assert!(!scheme.is_valid_for_create());
    }
}
