//! Port of `SqlReadReceiptStore` (channels/store/sqlstore/read_receipt_store.go), narrowed to
//! what `revealPost` and `burnPost` reach.
//!
//! `ReadReceipts` is one row per (post, reader) for a burn-on-read post: written the first time
//! a reader reveals it, carrying the instant the reader's copy expires. Three columns, a
//! composite primary key, and no foreign keys.
//!
//! # `GetUnreadCountForPost` counts **members**, not recipients
//!
//! `ChannelMembers LEFT JOIN ReadReceipts ON UserId AND PostId` for the post's channel, minus
//! the author, where the join found nothing. So a member added to the channel *after* the post
//! counts as unread, and a reader who left the channel after revealing does not count at all.
//! Go reads it from the master — the receipt it is asked about was written a statement ago.

use sqlx::PgPool;

use mm_model::read_receipt::ReadReceipt;

use crate::error::StoreError;

/// Port of `store.ReadReceiptStore`, narrowed to the four methods the reveal and burn reach.
pub trait ReadReceiptStore {
    /// Port of `SqlReadReceiptStore.Save` (read_receipt_store.go:46): a plain `INSERT`, so a
    /// second receipt for the same (post, user) is a primary-key violation, not an upsert.
    fn save(
        &self,
        receipt: &ReadReceipt,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlReadReceiptStore.Update` (read_receipt_store.go:64): `ExpireAt` only.
    /// Updating a row that is not there is not an error.
    fn update(
        &self,
        receipt: &ReadReceipt,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlReadReceiptStore.Get` (read_receipt_store.go:99): the one row, or
    /// [`StoreError::NotFound`] keyed `ReadReceipt` / `<post>_<user>`.
    fn get(
        &self,
        post_id: &str,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<ReadReceipt, StoreError>> + Send;

    /// Port of `SqlReadReceiptStore.GetByPost` (read_receipt_store.go:115): every receipt for
    /// the post, in no promised order.
    fn get_by_post(
        &self,
        post_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<ReadReceipt>, StoreError>> + Send;

    /// Port of `SqlReadReceiptStore.GetUnreadCountForPost` (read_receipt_store.go:143) — see
    /// the module docs for what is counted. Takes the three fields of the post it reads.
    fn get_unread_count_for_post(
        &self,
        post_id: &str,
        channel_id: &str,
        author_id: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;
}

#[derive(Debug, Clone)]
pub struct SqlReadReceiptStore {
    pool: PgPool,
}

impl SqlReadReceiptStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl ReadReceiptStore for SqlReadReceiptStore {
    #[tracing::instrument(skip(self, receipt), fields(post_id = %receipt.post_id, user_id = %receipt.user_id))]
    async fn save(&self, receipt: &ReadReceipt) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO readreceipts (postid, userid, expireat) VALUES ($1, $2, $3)",
            receipt.post_id,
            receipt.user_id,
            receipt.expire_at,
        )
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|source| StoreError::Db {
            context: "failed to save ReadReceipt".to_owned(),
            source,
        })
    }

    #[tracing::instrument(skip(self, receipt), fields(post_id = %receipt.post_id, user_id = %receipt.user_id))]
    async fn update(&self, receipt: &ReadReceipt) -> Result<(), StoreError> {
        sqlx::query!(
            "UPDATE readreceipts SET expireat = $3 WHERE postid = $1 AND userid = $2",
            receipt.post_id,
            receipt.user_id,
            receipt.expire_at,
        )
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|source| StoreError::Db {
            context: "failed to update ReadReceipt".to_owned(),
            source,
        })
    }

    #[tracing::instrument(skip(self), fields(post_id = %post_id, user_id = %user_id))]
    async fn get(&self, post_id: &str, user_id: &str) -> Result<ReadReceipt, StoreError> {
        let row = sqlx::query!(
            r#"SELECT postid AS "post_id!", userid AS "user_id!", expireat AS "expire_at!"
                 FROM readreceipts
                WHERE postid = $1 AND userid = $2"#,
            post_id,
            user_id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get ReadReceipt with id={post_id}_{user_id}"),
            source,
        })?;

        let Some(row) = row else {
            return Err(StoreError::NotFound {
                entity: "ReadReceipt",
                criteria: format!("{post_id}_{user_id}"),
            });
        };
        Ok(ReadReceipt {
            post_id: row.post_id,
            user_id: row.user_id,
            expire_at: row.expire_at,
        })
    }

    #[tracing::instrument(skip(self), fields(post_id = %post_id))]
    async fn get_by_post(&self, post_id: &str) -> Result<Vec<ReadReceipt>, StoreError> {
        let rows = sqlx::query!(
            r#"SELECT postid AS "post_id!", userid AS "user_id!", expireat AS "expire_at!"
                 FROM readreceipts
                WHERE postid = $1"#,
            post_id,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get ReadReceipts for postId={post_id}"),
            source,
        })?;
        Ok(rows
            .into_iter()
            .map(|row| ReadReceipt {
                post_id: row.post_id,
                user_id: row.user_id,
                expire_at: row.expire_at,
            })
            .collect())
    }

    #[tracing::instrument(skip(self), fields(post_id = %post_id, channel_id = %channel_id))]
    async fn get_unread_count_for_post(
        &self,
        post_id: &str,
        channel_id: &str,
        author_id: &str,
    ) -> Result<i64, StoreError> {
        sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!"
                 FROM channelmembers
                 LEFT JOIN readreceipts
                   ON channelmembers.userid = readreceipts.userid AND readreceipts.postid = $1
                WHERE channelmembers.channelid = $2
                  AND channelmembers.userid <> $3
                  AND readreceipts.userid IS NULL"#,
            post_id,
            channel_id,
            author_id,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to get unread count for postId={post_id} channelId={channel_id}"
            ),
            source,
        })
    }
}
