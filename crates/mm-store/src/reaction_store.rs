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

    /// Port of `SqlReactionStore.Save` (reaction_store.go:28).
    fn save(
        &self,
        reaction: &Reaction,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlReactionStore.Delete` (reaction_store.go:68).
    fn delete(
        &self,
        reaction: &Reaction,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlReactionStore.ExistsOnPost` (reaction_store.go:105).
    fn exists_on_post(
        &self,
        post_id: &str,
        emoji_name: &str,
    ) -> impl std::future::Future<Output = Result<bool, StoreError>> + Send;

    /// Port of `SqlReactionStore.GetUniqueCountForPost` (reaction_store.go:149).
    fn get_unique_count_for_post(
        &self,
        post_id: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// The `SELECT ChannelId FROM Posts WHERE Id = ?` inside `Save`'s transaction
    /// (reaction_store.go:41).
    ///
    /// Separate here because `SaveReactionForPost` pre-populates `ChannelId` from the post it has
    /// already fetched, so the query is reachable only from a caller that did not — and Go's
    /// comment on that line says as much ("get channelId, if not already populated").
    fn channel_id_for_post(
        &self,
        post_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<String>, StoreError>> + Send;
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

    /// # The insert is an upsert, and the transaction spans two tables
    ///
    /// Go's `saveReactionAndUpdatePost` writes the reaction with `ON CONFLICT (UserId, PostId,
    /// EmojiName) DO UPDATE`, then sets `Posts.HasReactions = True` and bumps `Posts.UpdateAt` —
    /// **both inside one transaction**, because a client that sees `HasReactions` false while the
    /// row exists renders a post with no reaction bar.
    ///
    /// The conflict clause is why re-reacting is not an error: it revives a soft-deleted row by
    /// writing `DeleteAt = 0` over it, keeping the original `CreateAt`. Go's caller has a
    /// `IsUniqueConstraintError` arm around this for the same case, but the `ON CONFLICT` target
    /// is exactly the primary key, so that arm is unreachable — it is not reproduced.
    ///
    /// `Posts.UpdateAt` is set from a **second** `GetMillis()` call in Go, taken after the
    /// reaction's own `PreSave`. The two can differ by a millisecond; this port takes its own
    /// reading at the same point for the same reason.
    #[tracing::instrument(skip(self, reaction), fields(post_id = %reaction.post_id, emoji = %reaction.emoji_name))]
    async fn save(&self, reaction: &Reaction) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "unable to begin the reaction save transaction".to_owned(),
            source,
        })?;

        sqlx::query!(
            r#"
            INSERT INTO reactions
                (userid, postid, emojiname, createat, updateat, deleteat, remoteid, channelid)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (userid, postid, emojiname)
                DO UPDATE SET updateat  = $5,
                              deleteat  = $6,
                              remoteid  = $7,
                              channelid = $8
            "#,
            reaction.user_id,
            reaction.post_id,
            reaction.emoji_name,
            reaction.create_at,
            reaction.update_at,
            reaction.delete_at,
            reaction.remote_id.as_deref().unwrap_or_default(),
            reaction.channel_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed while saving reaction".to_owned(),
            source,
        })?;

        sqlx::query!(
            r#"UPDATE posts SET hasreactions = TRUE, updateat = $1 WHERE id = $2"#,
            mm_model::utils::get_millis(),
            reaction.post_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed while updating post for reactions on insert".to_owned(),
            source,
        })?;

        tx.commit().await.map_err(|source| StoreError::Db {
            context: "unable to commit the reaction save transaction".to_owned(),
            source,
        })
    }

    /// # A delete is an UPDATE, and `HasReactions` is recomputed rather than cleared
    ///
    /// `deleteReactionAndUpdatePost` soft-deletes: `UpdateAt` and `DeleteAt` both take the
    /// reaction's `UpdateAt`, so a withdrawn reaction keeps its row and its `CreateAt`.
    ///
    /// The post update then sets `HasReactions` to `count(0) > 0` over the *remaining*
    /// undeleted reactions — not to `FALSE`. Removing the last of two reactions must leave the
    /// flag set, and a port that wrote `FALSE` here would clear the bar for every other
    /// reaction on the post.
    ///
    /// The `WHERE` has no `DeleteAt` predicate, so deleting an already-deleted reaction rewrites
    /// its timestamps and succeeds. Go returns no not-found error from this path at all.
    #[tracing::instrument(skip(self, reaction), fields(post_id = %reaction.post_id, emoji = %reaction.emoji_name))]
    async fn delete(&self, reaction: &Reaction) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "unable to begin the reaction delete transaction".to_owned(),
            source,
        })?;

        sqlx::query!(
            r#"
            UPDATE reactions
               SET updateat = $1, deleteat = $1, remoteid = $2
             WHERE postid = $3 AND userid = $4 AND emojiname = $5
            "#,
            reaction.update_at,
            reaction.remote_id.as_deref().unwrap_or_default(),
            reaction.post_id,
            reaction.user_id,
            reaction.emoji_name,
        )
        .execute(&mut *tx)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed while deleting reaction".to_owned(),
            source,
        })?;

        sqlx::query!(
            r#"
            UPDATE posts
               SET updateat     = $1,
                   hasreactions = (SELECT count(0) > 0
                                     FROM reactions
                                    WHERE postid = $2 AND COALESCE(deleteat, 0) = 0)
             WHERE id = $2
            "#,
            mm_model::utils::get_millis(),
            reaction.post_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed while updating post for reactions on delete".to_owned(),
            source,
        })?;

        tx.commit().await.map_err(|source| StoreError::Db {
            context: "unable to commit the reaction delete transaction".to_owned(),
            source,
        })
    }

    /// The `COALESCE(DeleteAt, 0) = 0` here is the same NULL-tolerance the read path carries: a
    /// row written before the backfill migration holds NULL, and without the coalesce it compares
    /// as unknown and the reaction reads as absent — which would let the unique-count limit be
    /// exceeded.
    #[tracing::instrument(skip(self), fields(post_id = %post_id, emoji = %emoji_name))]
    async fn exists_on_post(&self, post_id: &str, emoji_name: &str) -> Result<bool, StoreError> {
        let row = sqlx::query_scalar!(
            r#"
            SELECT 1 AS "one!"
              FROM reactions
             WHERE postid = $1 AND emojiname = $2 AND COALESCE(deleteat, 0) = 0
             LIMIT 1
            "#,
            post_id,
            emoji_name,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to check for existing reaction".to_owned(),
            source,
        })?;

        Ok(row.is_some())
    }

    /// **`DeleteAt = 0`, not `COALESCE(DeleteAt, 0) = 0`.**
    ///
    /// Go writes the bare comparison here and the coalesced one three functions above, in
    /// `ExistsOnPost`. That is an inconsistency in the Go source, not a transcription slip: on a
    /// database holding pre-migration NULLs the two disagree, and the count is the smaller. It is
    /// reproduced, because the number it produces is compared against a configured limit and
    /// "fixing" it would refuse reactions Go accepts.
    #[tracing::instrument(skip(self), fields(post_id = %post_id))]
    async fn get_unique_count_for_post(&self, post_id: &str) -> Result<i64, StoreError> {
        let count = sqlx::query_scalar!(
            r#"SELECT COUNT(DISTINCT emojiname) AS "count!" FROM reactions WHERE postid = $1 AND deleteat = 0"#,
            post_id,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to count Reactions".to_owned(),
            source,
        })?;

        Ok(count)
    }

    #[tracing::instrument(skip(self), fields(post_id = %post_id))]
    async fn channel_id_for_post(&self, post_id: &str) -> Result<Option<String>, StoreError> {
        sqlx::query_scalar!(
            r#"SELECT channelid AS "channel_id!" FROM posts WHERE id = $1"#,
            post_id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed while getting channelId from Posts".to_owned(),
            source,
        })
    }
}
