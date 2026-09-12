//! Port of `SqlEmojiStore` (channels/store/sqlstore/emoji_store.go): the reads, plus `Save` and
//! `Delete`.
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

    /// Port of `SqlEmojiStore.GetList` (emoji_store.go:74).
    ///
    /// `sort_by_name` is Go's `sort == model.EmojiSortByName`, resolved to a bool by the caller
    /// because that is the only value the string can usefully take — the handler 400s anything
    /// else and the empty string means "no ORDER BY at all".
    fn get_list(
        &self,
        offset: i64,
        limit: i64,
        sort_by_name: bool,
    ) -> impl std::future::Future<Output = Result<Vec<Emoji>, StoreError>> + Send;

    /// Port of `SqlEmojiStore.Search` (emoji_store.go:108).
    fn search(
        &self,
        name: &str,
        prefix_only: bool,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<Emoji>, StoreError>> + Send;

    /// Port of `SqlEmojiStore.Save` (emoji_store.go:38).
    ///
    /// **`PreSave` and `IsValid` run inside the store, not above it** — Go's `Save` opens with
    /// `emoji.PreSave()` and returns `IsValid`'s `*model.AppError` verbatim. That is why this
    /// takes the emoji **by value and hands it back**: the id, the lowercased name and the two
    /// timestamps are all minted here, and the caller needs the mutated row.
    ///
    /// `App.CreateEmoji` calls `PreSave`/`IsValid` a second time before it ever reaches this
    /// (app/emoji.go:54) so that a rejected emoji leaves no orphan image behind. The duplication
    /// is Go's and is reproduced: the second `PreSave` moves `CreateAt` again.
    fn save(
        &self,
        emoji: Emoji,
    ) -> impl std::future::Future<Output = Result<Emoji, StoreError>> + Send;

    /// Port of `SqlEmojiStore.Delete` (emoji_store.go:87) — a **soft** delete.
    ///
    /// `DeleteAt` and `UpdateAt` both take `time`, and the `AND DeleteAt = 0` in the predicate is
    /// what makes a second delete a **404** rather than a silent success: zero rows affected is
    /// `ErrNotFound`, which the app layer answers `app.emoji.delete.no_results` to.
    fn delete(
        &self,
        emoji_id: &str,
        time: i64,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;
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

    /// # Two statements, because the `ORDER BY` is the behaviour
    ///
    /// Go builds one `SelectBuilder` and appends `OrderBy("Name")` only when the caller asked
    /// for it, so the unsorted case really does reach Postgres with **no ordering clause** and
    /// the row order is whatever the plan yields. A parameterised `ORDER BY CASE WHEN $3 …`
    /// would collapse both halves into one statement a mutation could no longer flip, and would
    /// also impose an order Go does not ask for.
    ///
    /// # `LIMIT` and `OFFSET` are unconditional
    ///
    /// There is no `if limit > 0` guard here, unlike the channel and post pagination helpers:
    /// `?per_page=0` is `LIMIT 0` and answers the empty list rather than everything. Go casts
    /// both to `uint64`, so a negative value would wrap to something enormous — unreachable,
    /// because `web.ParamsFromRequest` floors both at zero before the app layer multiplies them.
    #[tracing::instrument(skip(self), fields(offset, limit, sort_by_name))]
    async fn get_list(
        &self,
        offset: i64,
        limit: i64,
        sort_by_name: bool,
    ) -> Result<Vec<Emoji>, StoreError> {
        let rows = if sort_by_name {
            sqlx::query_as!(
                EmojiRow,
                r#"
                SELECT id        AS "id!",
                       createat  AS "create_at!",
                       updateat  AS "update_at!",
                       deleteat  AS "delete_at!",
                       creatorid AS "creator_id!",
                       name      AS "name!"
                  FROM emoji
                 WHERE deleteat = 0
                 ORDER BY name
                 LIMIT $1 OFFSET $2
                "#,
                limit,
                offset
            )
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as!(
                EmojiRow,
                r#"
                SELECT id        AS "id!",
                       createat  AS "create_at!",
                       updateat  AS "update_at!",
                       deleteat  AS "delete_at!",
                       creatorid AS "creator_id!",
                       name      AS "name!"
                  FROM emoji
                 WHERE deleteat = 0
                 LIMIT $1 OFFSET $2
                "#,
                limit,
                offset
            )
            .fetch_all(&self.pool)
            .await
        }
        .map_err(|source| StoreError::Db {
            context: "could not get list of emojis".to_owned(),
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

    /// Port of `SqlEmojiStore.Search` (emoji_store.go:108).
    ///
    /// # The LIKE pattern is built, not bound
    ///
    /// Go sanitises the term with **backslash** as the escape character — not the `*` its channel
    /// search uses — and then concatenates: `term = ("" | "%") + name + "%"`. `sq.Like` emits a
    /// bare `Name LIKE ?` with **no `ESCAPE` clause**, so Postgres' default escape (backslash) is
    /// what makes the sanitising work. Reproduced exactly; an `ESCAPE '\'` would be equivalent
    /// but is not what Go writes.
    ///
    /// Two consequences a reader would not predict, both measured:
    ///
    /// - **A term of `\` matches everything.** `sanitizeSearchTerm` strips every occurrence of
    ///   the escape character first, so `\` sanitises to the empty string and the pattern becomes
    ///   the bare `%`.
    /// - **The match is case-sensitive.** There is no `LOWER` on either side, unlike the channel
    ///   autocomplete beside it, so `?name=MMRS` finds nothing while `?name=mmrs` finds the list.
    ///
    /// `emojiSelectQuery` carries `DeleteAt = 0`, so a deleted emoji is never a completion.
    /// `ORDER BY Name` and the caller's limit finish it.
    #[tracing::instrument(skip(self), fields(prefix_only, limit, found))]
    async fn search(
        &self,
        name: &str,
        prefix_only: bool,
        limit: i64,
    ) -> Result<Vec<Emoji>, StoreError> {
        let sanitized = sanitize_emoji_search_term(name);
        let pattern = if prefix_only {
            format!("{sanitized}%")
        } else {
            format!("%{sanitized}%")
        };

        let rows = sqlx::query_as!(
            EmojiRow,
            r#"
            SELECT id        AS "id!",
                   createat  AS "create_at!",
                   updateat  AS "update_at!",
                   deleteat  AS "delete_at!",
                   creatorid AS "creator_id!",
                   name      AS "name!"
              FROM emoji
             WHERE deleteat = 0
               AND name LIKE $1
             ORDER BY name
             LIMIT $2
            "#,
            pattern,
            limit
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("could not search emojis by name {sanitized}"),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());

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

    /// # The uniqueness is on `(Name, DeleteAt)`, which is not the constraint it looks like
    ///
    /// The schema carries `emoji_name_deleteat_key UNIQUE (name, deleteat)`, so two *live* rows
    /// cannot share a name — `DeleteAt` is 0 for both — and `App.CreateEmoji`'s `GetByName` check
    /// is therefore a nicer error message rather than the only guard. What it does **not** prevent
    /// is a name reused after a delete, which is the whole point of the composite: a deleted
    /// emoji keeps its name and a new one may take it.
    ///
    /// A losing race on that constraint arrives here as a plain [`StoreError::Db`], which
    /// `App.CreateEmoji` folds into its one 500 — Go does the same, wrapping every `Save` failure
    /// into `app.emoji.create.internal_error`. So a concurrent duplicate is a 500 on both servers,
    /// not the 400 the `GetByName` check gives the sequential case.
    ///
    /// A validation failure is [`StoreError::Invalid`], which carries the `AppError` **with its
    /// own status and id** (`model.emoji.*`, 400) all the way to the client — Go returns
    /// `IsValid`'s error unwrapped and `App.CreateEmoji` lets it through with `errors.As`.
    #[tracing::instrument(skip_all, fields(emoji_id, emoji_name))]
    async fn save(&self, mut emoji: Emoji) -> Result<Emoji, StoreError> {
        emoji.pre_save();
        if let Err(app_error) = emoji.is_valid() {
            return Err(StoreError::Invalid {
                entity: "Emoji",
                app_error,
            });
        }
        tracing::Span::current().record("emoji_id", &emoji.id);
        tracing::Span::current().record("emoji_name", &emoji.name);

        sqlx::query!(
            r#"
            INSERT INTO emoji (id, createat, updateat, deleteat, creatorid, name)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
            emoji.id,
            emoji.create_at,
            emoji.update_at,
            emoji.delete_at,
            emoji.creator_id,
            emoji.name,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "error saving emoji".to_owned(),
            source,
        })?;

        Ok(emoji)
    }

    /// # Zero rows is a miss, and the driver error is *discarded* when it is
    ///
    /// Go writes `else if rows, err := sqlResult.RowsAffected(); rows == 0` — the `err` from
    /// `RowsAffected` is only ever used to `Wrap` the not-found, never checked, so a row count of
    /// zero is `ErrNotFound` whatever else happened. sqlx has no such failure mode; the count is
    /// what decides here too.
    #[tracing::instrument(skip(self), fields(emoji_id = %emoji_id, time))]
    async fn delete(&self, emoji_id: &str, time: i64) -> Result<(), StoreError> {
        let result = sqlx::query!(
            r#"
            UPDATE emoji
               SET deleteat = $1,
                   updateat = $1
             WHERE id = $2
               AND deleteat = 0
            "#,
            time,
            emoji_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "could not delete emoji".to_owned(),
            source,
        })?;

        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound {
                entity: "Emoji",
                criteria: emoji_id.to_owned(),
            });
        }

        Ok(())
    }
}

/// Port of `sanitizeSearchTerm(term, "\\")` (sqlstore/utils.go:62) as the emoji store calls it.
///
/// The same function the channel search uses, with **backslash** as the escape character rather
/// than `*`. Order is Go's and it is the whole behaviour: every backslash is removed first, and
/// only then are `%` and `_` prefixed with one. So `\` sanitises to nothing, `%` to `\%`, and a
/// caller cannot smuggle a wildcard through either.
fn sanitize_emoji_search_term(term: &str) -> String {
    let mut out = term.replace('\\', "");
    for c in ['%', '_'] {
        out = out.replace(c, &format!("\\{c}"));
    }
    out
}

/// The six columns of `emojiSelectQuery` (emoji_store.go:27), named so the two `GetList`
/// statements — which differ only in their `ORDER BY` — share one row type. `sqlx::query!`
/// would give each branch its own anonymous record and they would not unify.
struct EmojiRow {
    id: String,
    create_at: i64,
    update_at: i64,
    delete_at: i64,
    creator_id: String,
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `sanitizeSearchTerm(term, "\\")`: the escape character is removed **before** `%` and `_`
    /// are escaped with it, so a term made only of backslashes sanitises to nothing — and an
    /// empty term makes the prefix pattern the bare `%`, which matches every emoji. Measured
    /// against the running server: `?name=%5C` returns the whole first page.
    #[test]
    fn the_escape_character_is_stripped_before_the_wildcards_are_escaped() {
        assert_eq!(sanitize_emoji_search_term(""), "");
        assert_eq!(sanitize_emoji_search_term("\\"), "");
        assert_eq!(sanitize_emoji_search_term("\\\\\\\\"), "");
        assert_eq!(sanitize_emoji_search_term("smile"), "smile");
        assert_eq!(sanitize_emoji_search_term("%"), "\\%");
        assert_eq!(sanitize_emoji_search_term("_"), "\\_");
        assert_eq!(sanitize_emoji_search_term("a%b_c"), "a\\%b\\_c");
        // A backslash the caller supplied cannot become an escape for the `%` beside it.
        assert_eq!(sanitize_emoji_search_term("\\%"), "\\%");
        // Case is untouched — the query has no `LOWER` on either side.
        assert_eq!(sanitize_emoji_search_term("SMILE"), "SMILE");
    }
}
