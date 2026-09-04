//! Port of `SqlEmojiStore` (channels/store/sqlstore/emoji_store.go): the three reads.
//!
//! `GetMultipleByName` unblocks `metadata.emojis` on `GET /api/v4/posts/{post_id}`, which is the
//! *custom* emoji used by a post's text and its reactions — system emoji never reach this table,
//! because the app layer filters them out before calling. `Get` and `GetByName` unblock
//! `GET /api/v4/emoji/{emoji_id}` and `GET /api/v4/emoji/name/{emoji_name}`.
//!
//! # One select builder, three readers, one shared predicate
//!
//! `emojiSelectQuery` (emoji_store.go:27) is constructed **with** `Where(sq.Eq{"DeleteAt": 0})`
//! and every reader inherits it. Reading any one method's body alone would miss the predicate
//! entirely and resurrect deleted custom emoji; it is written out in each query below rather
//! than factored into a helper, so a mutation can attack each copy independently.
//!
//! # Go's cache is not reproduced
//!
//! `Get` and `GetByName` take an `allowFromCache bool` that the SQL store ignores outright —
//! the memoisation lives in `LocalCacheEmojiStore`, a decorator this port has no equivalent of.
//! The consequence is a *staleness* difference, not a wire one: Go can answer from a
//! thirty-minute-old cache entry for a row that has since changed, and we always read the row.
//! Both servers agree on a settled database, which is what the parity suite asserts.

use mm_model::emoji::Emoji;
use sqlx::PgPool;

use crate::error::StoreError;

/// Port of `store.EmojiStore`, narrowed to the reads a migrated route makes.
pub trait EmojiStore {
    /// Port of `SqlEmojiStore.GetMultipleByName` (emoji_store.go:63).
    fn get_multiple_by_name(
        &self,
        names: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<Emoji>, StoreError>> + Send;

    /// Port of `SqlEmojiStore.Get` (emoji_store.go:55) — `getBy("Id", id)`.
    ///
    /// The `allowFromCache` parameter is not carried: the SQL store ignores it (see the module
    /// docs), so a parameter here would be one no implementation could act on.
    fn get(&self, id: &str) -> impl std::future::Future<Output = Result<Emoji, StoreError>> + Send;

    /// Port of `SqlEmojiStore.GetByName` (emoji_store.go:59) — `getBy("Name", name)`.
    fn get_by_name(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Result<Emoji, StoreError>> + Send;
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
    /// See the module docs: `emojiSelectQuery` carries the predicate and every reader inherits
    /// it. Reading only `GetMultipleByName`'s own body would resurrect deleted custom emoji
    /// into every post that once mentioned them.
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

    /// # `getBy` collapses two lookups into one function, and the error text is the difference
    ///
    /// Go writes `getBy(rctx, "Id", id)` and `getBy(rctx, "Name", name)` against one body, so a
    /// miss produces `store.NewErrNotFound("Emoji", "Id=<id>")` or `"Name=<name>"` — the column
    /// name, capitalised as the Go source spells it, not as Postgres folds it. Reproduced
    /// because it is what a log reader uses to tell the two routes apart; the app layer above
    /// gives them **different error ids** anyway (`app.emoji.get.no_result` against
    /// `app.emoji.get_by_name.no_result`), which is the part that reaches a client.
    #[tracing::instrument(skip(self), fields(emoji_id = %id))]
    async fn get(&self, id: &str) -> Result<Emoji, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT id        AS "id!",
                   createat  AS "create_at!",
                   updateat  AS "update_at!",
                   deleteat  AS "delete_at!",
                   creatorid AS "creator_id!",
                   name      AS "name!"
              FROM emoji
             WHERE deleteat = 0
               AND id = $1
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("could not get emoji by Id with value {id}"),
            source,
        })?
        .ok_or_else(|| StoreError::NotFound {
            entity: "Emoji",
            criteria: format!("Id={id}"),
        })?;

        Ok(Emoji {
            id: row.id,
            create_at: row.create_at,
            update_at: row.update_at,
            delete_at: row.delete_at,
            creator_id: row.creator_id,
            name: row.name,
        })
    }

    /// # `Name` is unique among the *live* rows only
    ///
    /// The table's uniqueness is enforced by Go's `Save` path, not by a database constraint, and
    /// a soft-deleted emoji keeps its name. So `WHERE deleteat = 0 AND name = $1` can in
    /// principle match more than one row across a delete-and-recreate — `GetBuilder` takes the
    /// first the scan yields, unordered, and so does `fetch_optional` here. Neither server picks
    /// deliberately, and adding an `ORDER BY` would make us disagree with Go on the pathological
    /// case while changing nothing on the ordinary one.
    #[tracing::instrument(skip(self), fields(emoji_name = %name))]
    async fn get_by_name(&self, name: &str) -> Result<Emoji, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT id        AS "id!",
                   createat  AS "create_at!",
                   updateat  AS "update_at!",
                   deleteat  AS "delete_at!",
                   creatorid AS "creator_id!",
                   name      AS "name!"
              FROM emoji
             WHERE deleteat = 0
               AND name = $1
            "#,
            name
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("could not get emoji by Name with value {name}"),
            source,
        })?
        .ok_or_else(|| StoreError::NotFound {
            entity: "Emoji",
            criteria: format!("Name={name}"),
        })?;

        Ok(Emoji {
            id: row.id,
            create_at: row.create_at,
            update_at: row.update_at,
            delete_at: row.delete_at,
            creator_id: row.creator_id,
            name: row.name,
        })
    }
}
