//! Port of `SqlEmojiStore` (channels/store/sqlstore/emoji_store.go), `GetMultipleByName` only.
//!
//! Unblocks `metadata.emojis` on `GET /api/v4/posts/{post_id}`, which is the *custom* emoji used
//! by a post's text and its reactions. System emoji never reach this table — the app layer
//! filters them out before calling (see `App::get_multiple_emoji_by_name`).

use mm_model::emoji::Emoji;
use sqlx::PgPool;

use crate::error::StoreError;

/// Port of `store.EmojiStore`, narrowed to the one read a post handler makes.
pub trait EmojiStore {
    /// Port of `SqlEmojiStore.GetMultipleByName` (emoji_store.go:63).
    fn get_multiple_by_name(
        &self,
        names: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<Emoji>, StoreError>> + Send;
}

#[derive(Debug, Clone)]
pub struct SqlEmojiStore {
    pool: PgPool,
}

impl SqlEmojiStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl EmojiStore for SqlEmojiStore {
    /// # `DeleteAt = 0` lives in the shared select builder, not in this method
    ///
    /// `emojiSelectQuery` (emoji_store.go:27) is constructed **with** `Where(sq.Eq{"DeleteAt":
    /// 0})`, and every reader — `Get`, `GetByName`, `GetList`, this one — inherits it. Reading
    /// only `GetMultipleByName`'s own body would miss the predicate entirely and resurrect
    /// deleted custom emoji into every post that once mentioned them.
    ///
    /// # There is no `ORDER BY`
    ///
    /// Go does not sort, so the array order is whatever Postgres returns. Reproduced rather than
    /// stabilised: adding an `ORDER BY` here would make us *disagree* with Go whenever its
    /// unordered scan happens to come back differently. A post using two custom emoji is
    /// therefore not order-stable across the two servers — see `MIGRATION.md`.
    #[tracing::instrument(skip(self), fields(count = names.len()))]
    async fn get_multiple_by_name(&self, names: &[String]) -> Result<Vec<Emoji>, StoreError> {
        let rows = sqlx::query!(
            r#"
            SELECT id        AS "id!",
                   createat  AS "create_at!",
                   updateat  AS "update_at!",
                   deleteat  AS "delete_at!",
                   creatorid AS "creator_id!",
                   name      AS "name!"
              FROM emoji
             WHERE deleteat = 0
               AND name = ANY($1)
            "#,
            names
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("error getting emojis by names {names:?}"),
            source,
        })?;

        Ok(rows
            .into_iter()
            .map(|row| Emoji {
                id: row.id,
                create_at: row.create_at,
                update_at: row.update_at,
                delete_at: row.delete_at,
                creator_id: row.creator_id,
                name: row.name,
            })
            .collect())
    }
}
