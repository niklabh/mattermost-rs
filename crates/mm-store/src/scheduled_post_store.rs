//! Port of `SqlScheduledPostStore` (channels/store/sqlstore/scheduled_post_store.go),
//! `PermanentDeleteByUser` only.
//!
//! Ported for `App.PermanentDeleteUser` (app/user.go:2134), which is the only caller that needs
//! it. The scheduled-post routes (`api4/scheduled_post.go`) are not served; their reads and
//! writes land here when one is.

use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `store.ScheduledPostStore` (store/store.go) that is ported.
pub trait ScheduledPostStore {
    /// Port of `SqlScheduledPostStore.PermanentDeleteByUser` (scheduled_post_store.go:355).
    ///
    /// Every scheduled post the user owns, sent, failed or pending alike. One `DELETE`, no
    /// `RowsAffected` check. Go also logs the failure at error level before returning it.
    fn permanent_delete_by_user(
        &self,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlScheduledPostStore {
    pool: PgPool,
}

impl SqlScheduledPostStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl ScheduledPostStore for SqlScheduledPostStore {
    #[tracing::instrument(skip_all, fields(user_id = %user_id, deleted))]
    async fn permanent_delete_by_user(&self, user_id: &str) -> Result<(), StoreError> {
        let result = sqlx::query!("DELETE FROM scheduledposts WHERE userid = $1", user_id)
            .execute(&self.pool)
            .await
            .map_err(|source| {
                tracing::error!(error = %source, "PermanentDeleteByUser: failed to delete scheduled posts by user from database");
                StoreError::Db {
                    context: "PermanentDeleteByUser: failed to delete scheduled posts by user from database".to_owned(),
                    source,
                }
            })?;
        tracing::Span::current().record("deleted", result.rows_affected());
        Ok(())
    }
}
