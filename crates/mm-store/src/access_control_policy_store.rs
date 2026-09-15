//! Port of `SqlAccessControlPolicyStore` (channels/store/sqlstore/access_control_policy_store.go),
//! restricted to the methods a build with a **nil** access-control service reaches.
//!
//! `Delete` came first: `cleanupTeamAccessControlPolicy` and `cleanupChannelAccessControlPolicy`
//! call it unconditionally on every archive and permanent delete. `Get`, `Save` and
//! `SearchPolicies` landed with `api4/access_control.go`, whose handlers reach the store **before**
//! the nil service is consulted: `ValidateTeamAdminPolicyOwnership` (two searches) decides a
//! team admin's 403 vs the service's 501, and `ReconcilePolicyTeamScope` (search, get, save) runs
//! on every `assign`/`unassign` that carries no resource ids — a 200 on this build. Everything
//! else in the interface (`SetActiveStatus*`, `GetAll`, the action aggregations) is only reached
//! through the service and is not ported.
//!
//! # The row is two JSON documents
//!
//! `Data` is `accessControlPolicyV0_1` — `imports`, `rules`, `roles`, `scope`, `scope_id` — and
//! `Props` is the free-form map; both are `jsonb`. [`StoredData`] reproduces the Go struct's
//! **field order and `omitempty` set**, because `Save` decides whether a revision changed by
//! comparing the *marshalled bytes* of the incoming policy against a re-marshal of the stored one
//! (`bytes.Equal(storePolicy.Data, tmp.Data)`): both sides go through the same marshal here too,
//! so the comparison is between two outputs of one function and never between a marshal and the
//! database's canonical jsonb text.

use mm_model::access_policy::{
    AccessControlPolicy, AccessControlPolicyRule, AccessControlPolicySearch,
};
use mm_model::utils::{StringInterface, go_json_marshal};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::error::StoreError;

/// `DefaultPerPage` / `MaxPerPage` (access_control_policy_store.go:20-21).
const DEFAULT_PER_PAGE: i64 = 10;
const MAX_PER_PAGE: i64 = 1000;

/// The subset of Go's `store.AccessControlPolicyStore` (store/store.go:1220) that is ported.
pub trait AccessControlPolicyStore {
    /// Port of `SqlAccessControlPolicyStore.Delete`: copy the policy row into
    /// `AccessControlPolicyHistory`, then delete it, in one transaction — or do nothing at all
    /// when no row carries the id. `Active` is the one column the history table does not have.
    ///
    /// The history insert has no `ON CONFLICT`: `(ID, Revision)` is its primary key, so deleting
    /// a policy whose current revision was already archived fails and rolls back, exactly as in
    /// Go — where the caller logs a warning and carries on.
    fn delete(&self, id: &str) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlAccessControlPolicyStore.Get` (:495): the row, or `NotFound`.
    fn get(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<AccessControlPolicy, StoreError>> + Send;

    /// Port of `SqlAccessControlPolicyStore.Save` (:188) — policies are immutable revisions.
    ///
    /// With an existing row: a different `Type` is refused; an unchanged `Data` + `Version` is
    /// **not** a new revision (a changed `Name` is updated in place and the stored row returned);
    /// otherwise the existing row moves to the history table, is deleted, and the new one is
    /// inserted at `Revision + 1` with `CreateAt` stamped now. Without one, the latest history
    /// row (if any) supplies the revision to continue from. `IsValid` runs first, as a
    /// [`StoreError::Invalid`].
    fn save(
        &self,
        policy: &AccessControlPolicy,
    ) -> impl std::future::Future<Output = Result<AccessControlPolicy, StoreError>> + Send;

    /// Port of `SqlAccessControlPolicyStore.SearchPolicies` (:619): the page and the total.
    ///
    /// Every filter is optional and they `AND` together. The ones a reader gets wrong: `Scope`
    /// and `ScopeID` filter only when **both** are set; `TeamID` on a non-`channel` type means
    /// "parent policies whose child channels all sit in that one team" (and forces
    /// `Type = parent` when no type was given); the cursor applies to the page but not to the
    /// count; `include_children` stamps `child_ids`/`channel_count`/`team_count` into `props`
    /// only when no `parent_id` is set.
    fn search_policies(
        &self,
        opts: &AccessControlPolicySearch,
    ) -> impl std::future::Future<Output = Result<(Vec<AccessControlPolicy>, i64), StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlAccessControlPolicyStore {
    pool: PgPool,
}

impl SqlAccessControlPolicyStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// `accessControlPolicyV0_1` (access_control_policy_store.go:25) — the `Data` document. Field
/// order and the `omitempty` set are Go's; `imports` is always written (nil becomes `[]` in
/// `fromModel`) and `rules` is written as `null` when nil.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct StoredData {
    imports: Option<Vec<String>>,
    rules: Option<Vec<AccessControlPolicyRule>>,
    #[serde(skip_serializing_if = "roles_omitted")]
    roles: Option<Vec<String>>,
    #[serde(skip_serializing_if = "String::is_empty")]
    scope: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    scope_id: String,
}

fn roles_omitted(roles: &Option<Vec<String>>) -> bool {
    roles.as_ref().is_none_or(Vec::is_empty)
}

/// One row of `AccessControlPolicies`, as `storeAccessControlPolicy` (:37).
#[derive(Debug, sqlx::FromRow)]
struct PolicyRow {
    id: String,
    name: String,
    #[sqlx(rename = "type")]
    type_: String,
    active: bool,
    createat: i64,
    revision: i32,
    version: String,
    data: Option<serde_json::Value>,
    props: Option<serde_json::Value>,
}

/// The `Data` and `Props` documents of a policy, marshalled the way `fromModel` (:81) does.
struct Marshalled {
    data: String,
    props: String,
}

/// `fromModel` (:81): the two documents as Go writes them. `roles`, `scope` and `scope_id` drop
/// out when empty; `imports` never does.
fn marshal(policy: &AccessControlPolicy) -> Result<Marshalled, StoreError> {
    let data = StoredData {
        imports: Some(policy.imports.clone().unwrap_or_default()),
        rules: policy.rules.clone(),
        roles: policy.roles.clone(),
        scope: policy.scope.clone(),
        scope_id: policy.scope_id.clone(),
    };
    let data = go_json_marshal(&data).map_err(|_| StoreError::Argument {
        entity: "AccessControlPolicy",
        detail: "failed to marshal the policy data",
    })?;
    let props = go_json_marshal(&policy.props).map_err(|_| StoreError::Argument {
        entity: "AccessControlPolicy",
        detail: "failed to marshal the policy props",
    })?;
    Ok(Marshalled { data, props })
}

/// The marshalled text as the jsonb parameter. Go hands the bytes over with the binary flag, so a
/// `null` document is a JSON null in the column, not a SQL NULL.
fn as_jsonb(text: &str) -> Result<serde_json::Value, StoreError> {
    serde_json::from_str(text).map_err(|_| StoreError::Argument {
        entity: "AccessControlPolicy",
        detail: "marshalled document is not JSON",
    })
}

/// `toModel` (:49): a row back into the model. A missing or `null` document leaves the fields at
/// their zero values; a `Props` that is not an object is the unmarshal failure Go reports.
fn to_model(row: PolicyRow) -> Result<AccessControlPolicy, StoreError> {
    let mut policy = AccessControlPolicy {
        id: row.id,
        name: row.name,
        type_: row.type_,
        active: row.active,
        create_at: row.createat,
        revision: i64::from(row.revision),
        version: row.version,
        ..AccessControlPolicy::default()
    };
    if let Some(data) = row.data.filter(|value| !value.is_null()) {
        let parsed: StoredData =
            serde_json::from_value(data).map_err(|_| StoreError::Argument {
                entity: "AccessControlPolicy",
                detail: "failed to parse the policy data",
            })?;
        policy.imports = parsed.imports;
        policy.rules = parsed.rules;
        policy.roles = parsed.roles;
        policy.scope = parsed.scope;
        policy.scope_id = parsed.scope_id;
    }
    policy.props = match row.props {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Object(map)) => Some(map),
        Some(_) => {
            return Err(StoreError::Argument {
                entity: "AccessControlPolicy",
                detail: "failed to parse the policy props",
            });
        }
    };
    Ok(policy)
}

fn db_error(context: impl Into<String>) -> impl FnOnce(sqlx::Error) -> StoreError {
    move |source| StoreError::Db {
        context: context.into(),
        source,
    }
}

/// `IsUniqueConstraintError(err, []string{"Name", "idx_accesscontrolpolicies_name_type"})` — the
/// partial unique index on `(Name, Type) WHERE Type = 'parent'`.
fn name_conflict_or_db(context: impl Into<String>) -> impl FnOnce(sqlx::Error) -> StoreError {
    move |source| {
        let unique = source
            .as_database_error()
            .and_then(|db| db.constraint())
            .is_some_and(|constraint| constraint == "idx_accesscontrolpolicies_name_type");
        if unique {
            StoreError::Conflict {
                resource: "AccessControlPolicy",
                source,
            }
        } else {
            StoreError::Db {
                context: context.into(),
                source,
            }
        }
    }
}

impl AccessControlPolicyStore for SqlAccessControlPolicyStore {
    #[tracing::instrument(skip(self), fields(id = id, existed))]
    async fn delete(&self, id: &str) -> Result<(), StoreError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(db_error("failed to start transaction"))?;

        // Go reads the row (`getT`), and only when it exists writes the history copy and
        // deletes. `INSERT … SELECT` folds the read and the copy into one statement whose row
        // count says whether anything existed; the delete then matches the same row or nothing.
        let copied = sqlx::query!(
            r#"
            INSERT INTO accesscontrolpolicyhistory (id, name, type, createat, revision, version, data, props)
            SELECT id, name, type, createat, revision, version, data, props
              FROM accesscontrolpolicies
             WHERE id = $1
            "#,
            id
        )
        .execute(&mut *tx)
        .await
        .map_err(db_error(format!("failed to save policy with id={id} to history")))?
        .rows_affected();
        tracing::Span::current().record("existed", copied > 0);

        if copied > 0 {
            sqlx::query!("DELETE FROM accesscontrolpolicies WHERE id = $1", id)
                .execute(&mut *tx)
                .await
                .map_err(db_error(format!("failed to delete policy with id={id}")))?;
        }

        tx.commit()
            .await
            .map_err(db_error("failed to commit transaction"))?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(id = id))]
    async fn get(&self, id: &str) -> Result<AccessControlPolicy, StoreError> {
        let row = sqlx::query_as!(
            PolicyRow,
            r#"
            SELECT id, name, type AS "type_", active, createat, revision, version, data, props
              FROM accesscontrolpolicies
             WHERE id = $1
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(db_error(format!("failed to find policy with id={id}")))?
        .ok_or_else(|| StoreError::NotFound {
            entity: "AccessControlPolicy",
            criteria: format!("id={id}"),
        })?;
        to_model(row)
    }

    #[tracing::instrument(skip_all, fields(id = %policy.id, existed, revision))]
    async fn save(&self, policy: &AccessControlPolicy) -> Result<AccessControlPolicy, StoreError> {
        if let Err(app_error) = policy.is_valid() {
            return Err(StoreError::Invalid {
                entity: "AccessControlPolicy",
                app_error,
            });
        }
        let id = policy.id.as_str();
        let incoming = marshal(policy)?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(db_error("failed to start transaction"))?;

        // `getT`: the current row, inside the transaction.
        let existing = sqlx::query_as!(
            PolicyRow,
            r#"
            SELECT id, name, type AS "type_", active, createat, revision, version, data, props
              FROM accesscontrolpolicies
             WHERE id = $1
            "#,
            id
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_error(format!("failed to fetch policy with id={id}")))?
        .map(to_model)
        .transpose()?;
        tracing::Span::current().record("existed", existing.is_some());

        // The revision to continue from: the live row, or failing that the newest history row —
        // "to make sure we are not overwriting an existing policy".
        let previous = match &existing {
            Some(existing) => {
                if existing.type_ != policy.type_ {
                    return Err(StoreError::Argument {
                        entity: "AccessControlPolicy",
                        detail: "cannot change type of existing policy",
                    });
                }

                // A change to `Name` alone is cosmetic: updated in place, no new revision.
                let stored = marshal(existing)?;
                if stored.data == incoming.data && existing.version == policy.version {
                    let mut unchanged = existing.clone();
                    if existing.name != policy.name {
                        sqlx::query!(
                            "UPDATE accesscontrolpolicies SET name = $1 WHERE id = $2",
                            policy.name,
                            id
                        )
                        .execute(&mut *tx)
                        .await
                        .map_err(name_conflict_or_db(format!(
                            "failed to update name for policy with id={id}"
                        )))?;
                        unchanged.name = policy.name.clone();
                        tx.commit().await.map_err(db_error("commit_transaction"))?;
                    }
                    tracing::Span::current().record("revision", unchanged.revision);
                    return Ok(unchanged);
                }

                // Move the existing revision to history, then delete it.
                let history_data = as_jsonb(&stored.data)?;
                let history_props = as_jsonb(&stored.props)?;
                sqlx::query!(
                    r#"
                    INSERT INTO accesscontrolpolicyhistory
                        (id, name, type, createat, revision, version, data, props)
                    VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                    "#,
                    existing.id,
                    existing.name,
                    existing.type_,
                    existing.create_at,
                    i32::try_from(existing.revision).unwrap_or(i32::MAX),
                    existing.version,
                    history_data,
                    history_props,
                )
                .execute(&mut *tx)
                .await
                .map_err(db_error(format!(
                    "failed to save policy with id={id} to history"
                )))?;
                sqlx::query!("DELETE FROM accesscontrolpolicies WHERE id = $1", id)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_error(format!("failed to delete policy with id={id}")))?;
                Some(existing.revision)
            }
            None => sqlx::query_scalar!(
                r#"
                SELECT revision
                  FROM accesscontrolpolicyhistory
                 WHERE id = $1
                 ORDER BY revision DESC
                 LIMIT 1
                "#,
                id
            )
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_error(format!("failed to fetch policy with id={id}")))?
            .map(i64::from),
        };

        // `preSaveAccessControlPolicy`: the timestamp is always overwritten, the revision continues.
        let create_at = mm_model::utils::get_millis();
        let revision = previous.map_or(1, |previous| previous + 1);
        tracing::Span::current().record("revision", revision);
        let data = as_jsonb(&incoming.data)?;
        let props = as_jsonb(&incoming.props)?;
        sqlx::query!(
            r#"
            INSERT INTO accesscontrolpolicies
                (id, name, type, active, createat, revision, version, data, props)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            "#,
            policy.id,
            policy.name,
            policy.type_,
            policy.active,
            create_at,
            i32::try_from(revision).unwrap_or(i32::MAX),
            policy.version,
            data,
            props,
        )
        .execute(&mut *tx)
        .await
        .map_err(name_conflict_or_db(format!(
            "failed to save policy with id={id}"
        )))?;

        tx.commit().await.map_err(db_error("commit_transaction"))?;

        // `storePolicy.toModel()`: the row as written — `imports` normalised to `[]`, the stamped
        // timestamp and revision.
        let mut saved = policy.clone();
        saved.imports = Some(policy.imports.clone().unwrap_or_default());
        saved.create_at = create_at;
        saved.revision = revision;
        Ok(saved)
    }

    #[tracing::instrument(skip_all, fields(type_ = %opts.type_, team_id = %opts.team_id, found, total))]
    async fn search_policies(
        &self,
        opts: &AccessControlPolicySearch,
    ) -> Result<(Vec<AccessControlPolicy>, i64), StoreError> {
        // `sanitizeSearchTerm(opts.Term, "*")` inside `%…%`, matched with `ESCAPE '*'`.
        let term = if opts.term.is_empty() {
            String::new()
        } else {
            format!(
                "%{}%",
                crate::user_store::sanitize_search_term(&opts.term, '*')
            )
        };
        let ids: Vec<String> = opts.ids.clone().unwrap_or_default();
        let actions: Vec<String> = opts.actions.clone().unwrap_or_default();
        let limit = if opts.limit < 1 {
            DEFAULT_PER_PAGE
        } else {
            opts.limit.min(MAX_PER_PAGE)
        };
        let with_children = opts.include_children && opts.parent_id.is_empty();

        struct SearchRow {
            id: String,
            name: String,
            type_: String,
            active: bool,
            createat: i64,
            revision: i32,
            version: String,
            data: Option<serde_json::Value>,
            props: Option<serde_json::Value>,
            child_ids: Option<serde_json::Value>,
            channel_count: Option<i64>,
            team_count: Option<i64>,
        }

        let rows = sqlx::query_as!(
            SearchRow,
            r#"
            SELECT p.id, p.name, p.type AS "type_", p.active, p.createat, p.revision, p.version,
                   p.data, p.props,
                   CASE WHEN $12::bool THEN COALESCE((SELECT JSON_AGG(c.id)
                        FROM accesscontrolpolicies c
                       WHERE c.type != 'parent'
                         AND c.data->'imports' @> JSONB_BUILD_ARRAY(p.id)), '[]'::json) END AS "child_ids: serde_json::Value",
                   CASE WHEN $12::bool THEN COALESCE((SELECT COUNT(*)
                        FROM accesscontrolpolicies c
                       WHERE c.type = 'channel'
                         AND c.data->'imports' @> JSONB_BUILD_ARRAY(p.id)), 0) END AS "channel_count",
                   CASE WHEN $12::bool THEN COALESCE((SELECT COUNT(*)
                        FROM accesscontrolpolicies c
                       WHERE c.type = 'team'
                         AND c.data->'imports' @> JSONB_BUILD_ARRAY(p.id)), 0) END AS "team_count"
              FROM accesscontrolpolicies p
             WHERE ($1::text = '' OR LOWER(p.name) LIKE LOWER($1) ESCAPE '*')
               AND ($2::text = '' OR p.type = $2)
               AND ($3::text = '' OR p.data->'imports' @> to_jsonb($3::text))
               AND (cardinality($4::text[]) = 0 OR EXISTS (
                        SELECT 1
                          FROM jsonb_array_elements(CASE WHEN jsonb_typeof(p.data->'rules') = 'array'
                                                         THEN p.data->'rules' ELSE '[]'::jsonb END) AS rule,
                               unnest($4::text[]) AS wanted(action)
                         WHERE rule->'actions' @> to_jsonb(wanted.action)))
               AND (NOT $5::bool OR p.active = true)
               AND (cardinality($6::text[]) = 0 OR p.id = ANY($6))
               AND ($7::text = '' OR $8::text = ''
                    OR (p.data->>'scope' = $7 AND p.data->>'scope_id' = $8))
               AND ($9::text = '' OR CASE
                    WHEN $2 = 'channel' THEN p.id IN (SELECT id FROM channels WHERE teamid = $9)
                    ELSE ($2 <> '' OR p.type = 'parent')
                         AND p.id IN (
                            SELECT parent_id FROM (
                                SELECT ch.teamid,
                                       jsonb_array_elements_text(COALESCE(NULLIF(cp.data->'imports', 'null'::jsonb), '[]'::jsonb)) AS parent_id
                                  FROM accesscontrolpolicies cp
                                  JOIN channels ch ON ch.id = cp.id
                                 WHERE cp.type = 'channel'
                            ) team_children
                            GROUP BY parent_id
                            HAVING COUNT(DISTINCT teamid) = 1 AND MIN(teamid) = $9)
                    END)
               AND ($10::text = '' OR p.id > $10)
             ORDER BY p.id ASC
             LIMIT $11
            "#,
            term,
            opts.type_,
            opts.parent_id,
            &actions,
            opts.active,
            &ids,
            opts.scope,
            opts.scope_id,
            opts.team_id,
            opts.cursor.id,
            limit,
            with_children,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_error(format!(
            "failed to find policies with opts={{\"name\"={:?}, \"resourceType\"={:?}",
            opts.term, opts.type_
        )))?;

        let mut policies = Vec::with_capacity(rows.len());
        for row in rows {
            let child_ids = row.child_ids;
            let channel_count = row.channel_count;
            let team_count = row.team_count;
            let mut policy = to_model(PolicyRow {
                id: row.id,
                name: row.name,
                type_: row.type_,
                active: row.active,
                createat: row.createat,
                revision: row.revision,
                version: row.version,
                data: row.data,
                props: row.props,
            })?;
            // "Props field is not guaranteed to be persisted correctly, and it shouldn't be" —
            // the child metadata is stamped over whatever the row carries.
            if with_children {
                let props = policy.props.get_or_insert_with(StringInterface::new);
                props.insert(
                    "child_ids".to_owned(),
                    child_ids.unwrap_or_else(|| serde_json::Value::Array(Vec::new())),
                );
                props.insert(
                    "channel_count".to_owned(),
                    serde_json::Value::from(channel_count.unwrap_or(0)),
                );
                props.insert(
                    "team_count".to_owned(),
                    serde_json::Value::from(team_count.unwrap_or(0)),
                );
            }
            policies.push(policy);
        }
        tracing::Span::current().record("found", policies.len());

        // The count carries every filter but the cursor and the limit.
        let total = sqlx::query_scalar!(
            r#"
            SELECT COUNT(*) AS "total!"
              FROM accesscontrolpolicies p
             WHERE ($1::text = '' OR LOWER(p.name) LIKE LOWER($1) ESCAPE '*')
               AND ($2::text = '' OR p.type = $2)
               AND ($3::text = '' OR p.data->'imports' @> to_jsonb($3::text))
               AND (cardinality($4::text[]) = 0 OR EXISTS (
                        SELECT 1
                          FROM jsonb_array_elements(CASE WHEN jsonb_typeof(p.data->'rules') = 'array'
                                                         THEN p.data->'rules' ELSE '[]'::jsonb END) AS rule,
                               unnest($4::text[]) AS wanted(action)
                         WHERE rule->'actions' @> to_jsonb(wanted.action)))
               AND (NOT $5::bool OR p.active = true)
               AND (cardinality($6::text[]) = 0 OR p.id = ANY($6))
               AND ($7::text = '' OR $8::text = ''
                    OR (p.data->>'scope' = $7 AND p.data->>'scope_id' = $8))
               AND ($9::text = '' OR CASE
                    WHEN $2 = 'channel' THEN p.id IN (SELECT id FROM channels WHERE teamid = $9)
                    ELSE ($2 <> '' OR p.type = 'parent')
                         AND p.id IN (
                            SELECT parent_id FROM (
                                SELECT ch.teamid,
                                       jsonb_array_elements_text(COALESCE(NULLIF(cp.data->'imports', 'null'::jsonb), '[]'::jsonb)) AS parent_id
                                  FROM accesscontrolpolicies cp
                                  JOIN channels ch ON ch.id = cp.id
                                 WHERE cp.type = 'channel'
                            ) team_children
                            GROUP BY parent_id
                            HAVING COUNT(DISTINCT teamid) = 1 AND MIN(teamid) = $9)
                    END)
            "#,
            term,
            opts.type_,
            opts.parent_id,
            &actions,
            opts.active,
            &ids,
            opts.scope,
            opts.scope_id,
            opts.team_id,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(db_error(format!(
            "failed to count policies with opts={{\"name\"={:?}, \"resourceType\"={:?}",
            opts.term, opts.type_
        )))?;
        tracing::Span::current().record("total", total);

        Ok((policies, total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `fromModel`: nil imports become `[]`, nil rules stay `null`, and the three `omitempty`
    /// fields drop out — in Go's field order, since `Save` compares the bytes.
    #[test]
    fn data_is_marshalled_as_go_writes_it() {
        let policy = AccessControlPolicy {
            id: "p".repeat(26),
            name: "n".to_owned(),
            type_: "parent".to_owned(),
            version: "v0.3".to_owned(),
            ..AccessControlPolicy::default()
        };
        let marshalled = marshal(&policy).unwrap();
        assert_eq!(marshalled.data, r#"{"imports":[],"rules":null}"#);
        assert_eq!(marshalled.props, "null");

        let scoped = AccessControlPolicy {
            imports: Some(vec!["a".to_owned()]),
            rules: Some(vec![]),
            roles: Some(vec![]),
            scope: "team".to_owned(),
            scope_id: "t".to_owned(),
            props: Some(StringInterface::new()),
            ..policy
        };
        let marshalled = marshal(&scoped).unwrap();
        assert_eq!(
            marshalled.data, r#"{"imports":["a"],"rules":[],"scope":"team","scope_id":"t"}"#,
            "empty roles is omitted like nil roles"
        );
        assert_eq!(marshalled.props, "{}");
    }

    /// `toModel`: a `null` document and a SQL NULL are both "nothing", a non-object `Props` is
    /// the unmarshal failure.
    #[test]
    fn rows_read_back_like_go() {
        let row = |data: Option<serde_json::Value>, props: Option<serde_json::Value>| PolicyRow {
            id: "id".to_owned(),
            name: "n".to_owned(),
            type_: "channel".to_owned(),
            active: true,
            createat: 5,
            revision: 3,
            version: "v0.3".to_owned(),
            data,
            props,
        };
        let policy = to_model(row(None, None)).unwrap();
        assert_eq!(policy.imports, None);
        assert_eq!(policy.props, None);
        assert_eq!(policy.revision, 3);

        let policy = to_model(row(
            Some(
                serde_json::json!({"imports": [], "rules": null, "scope": "team", "scope_id": "t"}),
            ),
            Some(serde_json::Value::Null),
        ))
        .unwrap();
        assert_eq!(policy.imports, Some(vec![]));
        assert_eq!(policy.rules, None);
        assert_eq!(policy.scope, "team");
        assert_eq!(policy.props, None);

        assert!(to_model(row(None, Some(serde_json::json!([1])))).is_err());
    }
}
