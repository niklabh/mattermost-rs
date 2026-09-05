//! Port of the read side of `SqlReactionStore` (channels/store/sqlstore/reaction_store.go).
//!
//! `GetForPost` unblocks `metadata.reactions` on `GET /api/v4/posts/{post_id}` and the
//! `/reactions` route beside it; `BulkGetForPosts` unblocks `POST /api/v4/posts/ids/reactions`,
//! which the webapp fires once per channel load.

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

    /// Port of `SqlReactionStore.BulkGetForPosts` (reaction_store.go:164).
    fn bulk_get_for_posts(
        &self,
        post_ids: &[String],
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

    /// Port of `SqlReactionStore.BulkGetForPosts` (reaction_store.go:164).
    ///
    /// The same six columns and the same two `COALESCE`s as [`Self::get_for_post`] — see there
    /// for why neither is decoration — with `PostId IN (…)` in place of `PostId = ?`.
    ///
    /// # An empty id list is an error, not an empty answer
    ///
    /// Go builds the `IN` list itself with `constructArrayArgs` (sqlstore/utils.go:139), which
    /// for zero ids emits the literal `()`. `PostId IN ()` is a **syntax error** in Postgres, so
    /// the query never runs and the store returns a driver error that
    /// `App::get_bulk_reactions_for_posts` turns into a 500. That path is reachable from the
    /// wire — `getBulkReactions` has no length check, unlike every other by-ids handler — so
    /// `POST /api/v4/posts/ids/reactions` with a body of `[]` (or of `null`, which
    /// `SortedArrayFromJSON` reduces to the same thing) answers **500**, measured against the
    /// running Go server.
    ///
    /// `= ANY('{}')` would have made that a silently empty `{}` with a 200. The guard is here
    /// rather than in the handler because that is where Go's failure lives, and the status Go
    /// gives it — 500, not 400 — depends on it staying a store error.
    #[tracing::instrument(skip(self, post_ids), fields(asked = post_ids.len(), found))]
    async fn bulk_get_for_posts(&self, post_ids: &[String]) -> Result<Vec<Reaction>, StoreError> {
        if post_ids.is_empty() {
            return Err(StoreError::Argument {
                entity: "Reaction",
                detail: "invalid list of post ids",
            });
        }

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
             WHERE postid = ANY($1::text[])
               AND COALESCE(deleteat, 0) = 0
             ORDER BY createat
            "#,
            post_ids
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to get Reactions".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());

        Ok(rows
            .into_iter()
            .map(|row| Reaction {
                user_id: row.user_id,
                post_id: row.post_id,
                emoji_name: row.emoji_name,
                create_at: row.create_at,
                update_at: row.update_at,
                delete_at: row.delete_at,
                remote_id: row.remote_id,
                channel_id: row.channel_id,
            })
            .collect())
    }
}
