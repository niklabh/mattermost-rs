//! Port of `SqlProductNoticesStore.View` (channels/store/sqlstore/product_notices_store.go:62),
//! the write behind `PUT /api/v4/system/notices/view`.
//!
//! `ProductNoticeViewState` is one row per `(UserId, NoticeId)` — the primary key — holding how
//! many times the user has seen the notice and when. `Timestamp` is Unix **seconds**
//! (`time.Now().UTC().Unix()`), the third non-millisecond column in the migrated surface after
//! `PostReminders.TargetTime` and `DesktopTokens.CreateAt`.
//!
//! `GetViews` and `Clear` are not ported: the first is read by `GetProductNotices`, which needs
//! the notice cache the fetch job fills, and the second by the same job.

use sqlx::PgPool;

use crate::error::StoreError;

/// Port of `store.ProductNoticesStore`, narrowed to the one write the route reaches.
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
