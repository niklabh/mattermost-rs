//! Port of `SqlPostAcknowledgementStore` (channels/store/sqlstore/post_acknowledgements_store.go):
//! `Get`, `SaveWithModel` and `Delete` — the three the acknowledgement pair in `api4/post.go`
//! reaches. The reads a post list needs (`GetForPost`, `GetForPosts`) live in
//! [`crate::post_store::PostStore`], where the post metadata that consumes them is built.
//!
//! # A delete is an update
//!
//! `Delete` does not remove the row: it sets `AcknowledgedAt = 0`, and every read filters
//! `AcknowledgedAt != 0`. So a re-acknowledgement is the upsert's `ON CONFLICT` arm writing a new
//! timestamp over the zero, and the `(postid, userid)` primary key never sees a second row.
//!
//! # Both writes touch `Posts.UpdateAt`
//!
//! Each runs in a transaction with `updatePost` (post_acknowledgements_store.go:246), which stamps
//! the post's `UpdateAt` with a fresh `GetMillis()` — a second one, later than the acknowledgement's
//! own. That is why an acknowledgement moves the post in every "since" listing.

use mm_model::post_acknowledgement::PostAcknowledgement;
use mm_model::utils::get_millis;
use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `store.PostAcknowledgementStore` (store/store.go) that is ported.
pub trait PostAcknowledgementStore {
    /// Port of `SqlPostAcknowledgementStore.Get` (post_acknowledgements_store.go:25): the live
    /// acknowledgement for one `(post, user)`, or `ErrNotFound` — a zeroed row counts as absent.
    fn get(
        &self,
        post_id: &str,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<PostAcknowledgement, StoreError>> + Send;

    /// Port of `SqlPostAcknowledgementStore.SaveWithModel` (post_acknowledgements_store.go:47):
    /// `IsValid`, `PreSave`, then the upsert and the post stamp in one transaction. Taken by
    /// value and handed back because `PreSave` fills `acknowledged_at`.
    fn save_with_model(
        &self,
        acknowledgement: PostAcknowledgement,
    ) -> impl std::future::Future<Output = Result<PostAcknowledgement, StoreError>> + Send;

    /// Port of `SqlPostAcknowledgementStore.Delete` (post_acknowledgements_store.go:87): zero the
    /// timestamp and stamp the post, in one transaction.
    fn delete(
        &self,
        acknowledgement: &PostAcknowledgement,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlPostAcknowledgementStore {
    pool: PgPool,
}

impl SqlPostAcknowledgementStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// `updatePost` (post_acknowledgements_store.go:246), inside the caller's transaction.
async fn update_post(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    post_id: &str,
) -> Result<(), StoreError> {
    sqlx::query!(
        "UPDATE posts SET updateat = $1 WHERE id = $2",
        get_millis(),
        post_id
    )
    .execute(&mut **tx)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to update Post with id={post_id}"),
        source,
    })?;
    Ok(())
}

impl PostAcknowledgementStore for SqlPostAcknowledgementStore {
    /// `channelid` and `remoteid` were added by migration 000141 with `DEFAULT ''`, so a row
    /// written before it holds `''` rather than NULL; both are coalesced the same way Go's
    /// `string` scan would read them. `remoteid` is `*string` in Go, so an empty string stays
    /// `Some("")` — which `omitempty` then keeps on the wire, as Go does.
    #[tracing::instrument(skip(self), fields(post_id = post_id, user_id = user_id))]
    async fn get(&self, post_id: &str, user_id: &str) -> Result<PostAcknowledgement, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT postid                    AS "postid!",
                   userid                    AS "userid!",
                   COALESCE(channelid, '')   AS "channelid!",
                   COALESCE(acknowledgedat, 0) AS "acknowledgedat!",
                   remoteid
              FROM postacknowledgements
             WHERE postid = $1 AND userid = $2 AND acknowledgedat <> 0
            "#,
            post_id,
            user_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get PostAcknowledgement for postId={post_id}"),
            source,
        })?;

        match row {
            Some(row) => Ok(PostAcknowledgement {
                user_id: row.userid,
                post_id: row.postid,
                acknowledged_at: row.acknowledgedat,
                channel_id: row.channelid,
                remote_id: row.remoteid,
            }),
            None => Err(StoreError::NotFound {
                entity: "PostAcknowledgement",
                criteria: post_id.to_owned(),
            }),
        }
    }

    #[tracing::instrument(skip_all, fields(post_id = %acknowledgement.post_id, user_id = %acknowledgement.user_id))]
    async fn save_with_model(
        &self,
        mut acknowledgement: PostAcknowledgement,
    ) -> Result<PostAcknowledgement, StoreError> {
        if let Err(app_error) = acknowledgement.is_valid() {
            return Err(StoreError::Invalid {
                entity: "PostAcknowledgement",
                app_error,
            });
        }
        acknowledgement.pre_save();

        let mut tx = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "begin_transaction".to_owned(),
            source,
        })?;

        // `ON CONFLICT (postid, userid) DO UPDATE SET AcknowledgedAt = ?` — only the timestamp
        // moves on a re-acknowledgement; the channel and remote id stay as first written.
        sqlx::query!(
            r#"
            INSERT INTO postacknowledgements (postid, userid, channelid, acknowledgedat, remoteid)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (postid, userid) DO UPDATE SET acknowledgedat = $4
            "#,
            acknowledgement.post_id,
            acknowledgement.user_id,
            acknowledgement.channel_id,
            acknowledgement.acknowledged_at,
            acknowledgement.remote_id.as_deref()
        )
        .execute(&mut *tx)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to upsert PostAcknowledgement for postId={}",
                acknowledgement.post_id
            ),
            source,
        })?;

        update_post(&mut tx, &acknowledgement.post_id).await?;

        tx.commit().await.map_err(|source| StoreError::Db {
            context: "commit_transaction".to_owned(),
            source,
        })?;
        Ok(acknowledgement)
    }

    #[tracing::instrument(skip_all, fields(post_id = %acknowledgement.post_id, user_id = %acknowledgement.user_id))]
    async fn delete(&self, acknowledgement: &PostAcknowledgement) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "begin_transaction".to_owned(),
            source,
        })?;

        sqlx::query!(
            "UPDATE postacknowledgements SET acknowledgedat = 0 WHERE postid = $1 AND userid = $2",
            acknowledgement.post_id,
            acknowledgement.user_id
        )
        .execute(&mut *tx)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to delete PostAcknowledgement for postId={}",
                acknowledgement.post_id
            ),
            source,
        })?;

        update_post(&mut tx, &acknowledgement.post_id).await?;

        tx.commit().await.map_err(|source| StoreError::Db {
            context: "commit_transaction".to_owned(),
            source,
        })?;
        Ok(())
    }
}
