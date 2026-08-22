//! Port of `SqlReactionStore` (channels/store/sqlstore/reaction_store.go), `GetForPost` only.
//!
//! Unblocks `metadata.reactions` on `GET /api/v4/posts/{post_id}`.

use mm_model::reaction::Reaction;
use sqlx::PgPool;

use crate::error::StoreError;

/// Port of `store.ReactionStore`, narrowed to the one read a post handler makes.
pub trait ReactionStore {
    /// Port of `SqlReactionStore.GetForPost` (reaction_store.go:89).
    fn get_for_post(
        &self,
        post_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<Reaction>, StoreError>> + Send;
}

#[derive(Debug, Clone)]
pub struct SqlReactionStore {
    pool: PgPool,
}

impl SqlReactionStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl ReactionStore for SqlReactionStore {
    /// # Two `COALESCE`s that are not decoration
    ///
    /// `UpdateAt` falls back to **`CreateAt`**, not to zero, and `DeleteAt` falls back to `0`.
    /// Both columns predate a migration that backfilled them, so rows written by an older
    /// server still hold NULL. Dropping either coalesce turns those rows into a scan error
    /// rather than a value — and the `UpdateAt` one would additionally put `0` on the wire
    /// where Go puts the creation time.
    ///
    /// The `DeleteAt` coalesce appears **twice**: once in the select list and once in the
    /// predicate. A reaction is soft-deleted, so the predicate is what excludes withdrawn
    /// reactions; without the coalesce there, a NULL row would compare as unknown and vanish
    /// from a result that Go includes.
    ///
    /// `ORDER BY CreateAt` is wire surface — `metadata.reactions` is a JSON array.
    #[tracing::instrument(skip(self), fields(post_id = %post_id))]
    async fn get_for_post(&self, post_id: &str) -> Result<Vec<Reaction>, StoreError> {
        let rows = sqlx::query!(
            r#"
            SELECT userid    AS "user_id!",
                   postid    AS "post_id!",
                   emojiname AS "emoji_name!",
                   createat  AS "create_at!",
                   COALESCE(updateat, createat) AS "update_at!",
                   COALESCE(deleteat, 0)        AS "delete_at!",
                   remoteid  AS "remote_id?",
                   channelid AS "channel_id!"
              FROM reactions
             WHERE postid = $1
               AND COALESCE(deleteat, 0) = 0
             ORDER BY createat
            "#,
            post_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get Reactions with postId={post_id}"),
            source,
        })?;

        Ok(rows
            .into_iter()
            .map(|row| Reaction {
                user_id: row.user_id,
                post_id: row.post_id,
                emoji_name: row.emoji_name,
                // `CreateAt` carries no coalesce in Go either, and `Reaction.CreateAt` is a
                // plain `int64` — so a NULL is a scan failure on both servers rather than a
                // zero. Asserted non-null in the query for exactly that reason.
                create_at: row.create_at,
                update_at: row.update_at,
                delete_at: row.delete_at,
                remote_id: row.remote_id,
                channel_id: row.channel_id,
            })
            .collect())
    }
}
