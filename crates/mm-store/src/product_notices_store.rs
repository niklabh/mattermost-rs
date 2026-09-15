//! Port of `SqlProductNoticesStore.View` (channels/store/sqlstore/product_notices_store.go:62),
//! the write behind `PUT /api/v4/system/notices/view`.
//!
//! `ProductNoticeViewState` is one row per `(UserId, NoticeId)` — the primary key — holding how
//! many times the user has seen the notice and when. `Timestamp` is Unix **seconds**
//! (`time.Now().UTC().Unix()`), the third non-millisecond column in the migrated surface after
//! `PostReminders.TargetTime` and `DesktopTokens.CreateAt`.
//!
//! `GetViews` and `ClearOldNotices` are ported for `GetProductNotices` and the notice-cache
//! refresh; `Clear` (the per-id delete) has no caller on any served route.

use sqlx::PgPool;

use crate::error::StoreError;

/// Port of `store.ProductNoticesStore`: the view write, the view read and the stale-row sweep.
pub trait ProductNoticesStore {
    /// Port of `SqlProductNoticesStore.View` (product_notices_store.go:62): in one transaction,
    /// the user's existing rows among `notices` get `Viewed + 1` and a fresh `Timestamp`, and
    /// every id without a row gets one with `Viewed = 1`. An empty list — which is what a `null`
    /// or `[]` body decodes to — selects nothing, inserts nothing and still commits.
    fn view(
        &self,
        user_id: &str,
        notices: &[String],
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlProductNoticesStore.GetViews` (product_notices_store.go:121): every view row
    /// of one user, in no particular order — Go's query has no `ORDER BY`, and the caller looks
    /// rows up by notice id.
    fn get_views(
        &self,
        user_id: &str,
    ) -> impl std::future::Future<
        Output = Result<Vec<mm_model::product_notices::ProductNoticeViewState>, StoreError>,
    > + Send;

    /// Port of `SqlProductNoticesStore.ClearOldNotices` (product_notices_store.go:46): drop the
    /// view rows of every notice **not** in the feed. **An empty feed drops every row**: squirrel
    /// renders `sq.NotEq` over an empty slice as `(1=1)`, and the `cardinality` arm here is that
    /// rendering.
    fn clear_old_notices(
        &self,
        current: &mm_model::product_notices::ProductNotices,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;
}

#[derive(Debug, Clone)]
pub struct SqlProductNoticesStore {
    pool: PgPool,
}

impl SqlProductNoticesStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl ProductNoticesStore for SqlProductNoticesStore {
    #[tracing::instrument(skip(self), fields(user_id = %user_id, views))]
    async fn get_views(
        &self,
        user_id: &str,
    ) -> Result<Vec<mm_model::product_notices::ProductNoticeViewState>, StoreError> {
        let views = sqlx::query_as!(
            mm_model::product_notices::ProductNoticeViewState,
            r#"
            SELECT userid AS "user_id!", noticeid AS "notice_id!",
                   viewed AS "viewed!", "timestamp" AS "timestamp!"
              FROM productnoticeviewstate
             WHERE userid = $1
            "#,
            user_id,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get ProductNoticeViewState with userId={user_id}"),
            source,
        })?;
        tracing::Span::current().record("views", views.len());
        Ok(views)
    }

    #[tracing::instrument(skip_all, fields(current = current.0.len(), deleted))]
    async fn clear_old_notices(
        &self,
        current: &mm_model::product_notices::ProductNotices,
    ) -> Result<(), StoreError> {
        let ids: Vec<String> = current.0.iter().map(|n| n.id.clone()).collect();
        let result = sqlx::query!(
            r#"
            DELETE FROM productnoticeviewstate
             WHERE cardinality($1::text[]) = 0
                OR NOT (noticeid = ANY($1))
            "#,
            &ids,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to delete records from ProductNoticeViewState".to_owned(),
            source,
        })?;
        tracing::Span::current().record("deleted", result.rows_affected());
        Ok(())
    }

    #[tracing::instrument(skip(self, notices), fields(user_id = %user_id, notices = notices.len()))]
    async fn view(&self, user_id: &str, notices: &[String]) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "begin_transaction".to_owned(),
            source,
        })?;

        let existing: Vec<String> = sqlx::query_scalar!(
            r#"
            SELECT noticeid AS "notice_id!"
              FROM productnoticeviewstate
             WHERE userid = $1 AND noticeid = ANY($2)
            "#,
            user_id,
            notices,
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get ProductNoticeViewState with userId={user_id}"),
            source,
        })?;

        // `time.Now().UTC().Unix()` — seconds, once for the whole batch.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or_default();

        // update existing records
        for notice_id in &existing {
            sqlx::query!(
                r#"
                UPDATE productnoticeviewstate
                   SET viewed = viewed + 1, "timestamp" = $3
                 WHERE userid = $1 AND noticeid = $2
                "#,
                user_id,
                notice_id,
                now,
            )
            .execute(&mut *tx)
            .await
            .map_err(|source| StoreError::Db {
                context: "failed to update ProductNoticeViewState".to_owned(),
                source,
            })?;
        }

        // add new ones
        for notice_id in notices.iter().filter(|id| !existing.contains(id)) {
            sqlx::query!(
                r#"
                INSERT INTO productnoticeviewstate (userid, noticeid, viewed, "timestamp")
                VALUES ($1, $2, 1, $3)
                "#,
                user_id,
                notice_id,
                now,
            )
            .execute(&mut *tx)
            .await
            .map_err(|source| StoreError::Db {
                context: "failed to insert ProductNoticeViewState".to_owned(),
                source,
            })?;
        }

        tx.commit().await.map_err(|source| StoreError::Db {
            context: "commit_transaction".to_owned(),
            source,
        })
    }
}
