//! Port of `SqlAccessControlPolicyStore.Delete` (channels/store/sqlstore/access_control_policy_store.go:309).
//!
//! The one method of the attribute-based-access-control store a Team Edition or Enterprise
//! (below Advanced) server reaches: `cleanupTeamAccessControlPolicy` and
//! `cleanupChannelAccessControlPolicy` call it on every archive and every permanent delete, and
//! they call it **unconditionally** — the access-control *service* is nil on the open-source
//! build, so the store fallback is the path, and the underlying delete is a no-op when no row
//! exists. Everything else in the interface (`Save`, `Get`, `SearchPolicies`, the CEL evaluation)
//! is behind `MinimumEnterpriseAdvancedLicense` and is not ported.

use sqlx::PgPool;

use crate::error::StoreError;

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

impl AccessControlPolicyStore for SqlAccessControlPolicyStore {
    #[tracing::instrument(skip(self), fields(id = id, existed))]
    async fn delete(&self, id: &str) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "failed to start transaction".to_owned(),
            source,
        })?;

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
        .map_err(|source| StoreError::Db {
            context: format!("failed to save policy with id={id} to history"),
            source,
        })?
        .rows_affected();
        tracing::Span::current().record("existed", copied > 0);

        if copied > 0 {
            sqlx::query!("DELETE FROM accesscontrolpolicies WHERE id = $1", id)
                .execute(&mut *tx)
                .await
                .map_err(|source| StoreError::Db {
                    context: format!("failed to delete policy with id={id}"),
                    source,
                })?;
        }

        tx.commit().await.map_err(|source| StoreError::Db {
            context: "failed to commit transaction".to_owned(),
            source,
        })?;
        Ok(())
    }
}
