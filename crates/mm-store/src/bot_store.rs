//! Port of `SqlBotStore` (channels/store/sqlstore/bot_store.go) — the two reads and the two
//! writes.
//!
//! Ported for `getBot` (`GET /api/v4/bots/{bot_user_id}`), `getBots` (`GET /api/v4/bots`),
//! `createBot` (`POST /api/v4/bots`) and the three routes that go through `Update`: `patchBot`,
//! `updateBotActive` and `assignBot`.
//!
//! # A bot is half a join
//!
//! `Bots` holds only `UserId`, `Description`, `OwnerId` and the timestamps. **`Username` and
//! `DisplayName` come from `Users`** — and `DisplayName` is `u.FirstName`, not a column of its
//! own. Both queries therefore join `Users`, and a `Bots` row whose user is gone is invisible
//! through this store rather than appearing with an empty username.

use mm_model::bot::{Bot, BotGetOptions, BotList};
use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `store.BotStore` the two read routes need.
pub trait BotStore {
    /// Port of `SqlBotStore.Get` (bot_store.go:70). `ErrNotFound` on a miss.
    ///
    /// `include_deleted` **widens** the query by dropping `AND b.DeleteAt = 0`; it is not a filter
    /// for deleted bots.
    fn get(
        &self,
        bot_user_id: &str,
        include_deleted: bool,
    ) -> impl std::future::Future<Output = Result<Bot, StoreError>> + Send;

    /// Port of `SqlBotStore.GetAll` (bot_store.go:107).
    ///
    /// Go returns `bots := []*model.Bot{}` — the **empty-slice** initialiser — so a page with no
    /// rows marshals as `[]` and never `null`. Compare `mm_store::JobStore`, where the two
    /// initialisers sit side by side and are visible on the wire.
    fn get_all(
        &self,
        options: &BotGetOptions,
    ) -> impl std::future::Future<Output = Result<BotList, StoreError>> + Send;

    /// Port of `SqlBotStore.Save` (bot_store.go:164).
    ///
    /// "It assumes the corresponding user was saved via the user store" — the `Users` row must
    /// already exist, because [`BotStore::get`] inner-joins it and would not find this bot
    /// otherwise. `App.CreateBot` therefore saves the user first and deletes it again if this
    /// call fails.
    ///
    /// `PreSave` then `IsValid`, in that order: `PreSave` fills `CreateAt`/`UpdateAt`, and
    /// `IsValid` rejects a zero in either — so validating first would fail every create.
    fn save(&self, bot: &Bot) -> impl std::future::Future<Output = Result<Bot, StoreError>> + Send;

    /// Port of `SqlBotStore.Update` (bot_store.go:184).
    ///
    /// # It re-reads, and the read is what the caller gets back
    ///
    /// Go copies five fields — `Description`, `OwnerId`, `LastIconUpdate`, `UpdateAt`,
    /// `DeleteAt` — onto the row it just read and returns **that**, so `Username`,
    /// `DisplayName` and `CreateAt` in the answer come from the join and not from the caller's
    /// in-memory bot. `App.PatchBot` depends on the ordering: it writes the `Users` row *first*,
    /// so the username this re-read picks up is the patched one. Writing the bot first would
    /// answer with the old username while having stored the new one.
    ///
    /// A miss here is `NotFound`, which the app layer turns back into the same 404 a read gets.
    fn update(
        &self,
        bot: &Bot,
    ) -> impl std::future::Future<Output = Result<Bot, StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlBotStore {
    pool: PgPool,
}

impl SqlBotStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// One row of the nine-column projection both queries share.
///
/// The `COALESCE`s are on the **projection** only. Go scans into non-pointer `string`/`int64`, so
/// a NULL is a scan error there and the zero value is what its struct would have held; the
/// predicates below keep Go's exact `= 0` / `!= 0` comparisons, which a NULL fails on both sides.
struct BotRow {
    userid: String,
    username: String,
    displayname: String,
    description: String,
    ownerid: String,
    lasticonupdate: i64,
    createat: i64,
    updateat: i64,
    deleteat: i64,
}

impl From<BotRow> for Bot {
    fn from(row: BotRow) -> Self {
        Bot {
            user_id: row.userid,
            username: row.username,
            display_name: row.displayname,
            description: row.description,
            owner_id: row.ownerid,
            last_icon_update: row.lasticonupdate,
            create_at: row.createat,
            update_at: row.updateat,
            delete_at: row.deleteat,
        }
    }
}

impl BotStore for SqlBotStore {
    #[tracing::instrument(skip_all, fields(bot_user_id = %bot_user_id, include_deleted))]
    async fn get(&self, bot_user_id: &str, include_deleted: bool) -> Result<Bot, StoreError> {
        // Two literals rather than one predicate built at runtime: `query_as!` checks a literal,
        // and Go's own difference here is the presence or absence of a whole clause.
        let row = if include_deleted {
            sqlx::query_as!(
                BotRow,
                r#"
                SELECT b.userid                       AS "userid!",
                       COALESCE(u.username, '')       AS "username!",
                       COALESCE(u.firstname, '')      AS "displayname!",
                       COALESCE(b.description, '')    AS "description!",
                       COALESCE(b.ownerid, '')        AS "ownerid!",
                       COALESCE(b.lasticonupdate, 0)  AS "lasticonupdate!",
                       COALESCE(b.createat, 0)        AS "createat!",
                       COALESCE(b.updateat, 0)        AS "updateat!",
                       COALESCE(b.deleteat, 0)        AS "deleteat!"
                  FROM bots b
                  JOIN users u ON u.id = b.userid
                 WHERE b.userid = $1
                "#,
                bot_user_id
            )
            .fetch_optional(&self.pool)
            .await
        } else {
            sqlx::query_as!(
                BotRow,
                r#"
                SELECT b.userid                       AS "userid!",
                       COALESCE(u.username, '')       AS "username!",
                       COALESCE(u.firstname, '')      AS "displayname!",
                       COALESCE(b.description, '')    AS "description!",
                       COALESCE(b.ownerid, '')        AS "ownerid!",
                       COALESCE(b.lasticonupdate, 0)  AS "lasticonupdate!",
                       COALESCE(b.createat, 0)        AS "createat!",
                       COALESCE(b.updateat, 0)        AS "updateat!",
                       COALESCE(b.deleteat, 0)        AS "deleteat!"
                  FROM bots b
                  JOIN users u ON u.id = b.userid
                 WHERE b.userid = $1
                   AND b.deleteat = 0
                "#,
                bot_user_id
            )
            .fetch_optional(&self.pool)
            .await
        }
        .map_err(|source| StoreError::Db {
            context: format!("selectone: user_id={bot_user_id}"),
            source,
        })?;

        row.map(Bot::from).ok_or_else(|| StoreError::NotFound {
            entity: "Bot",
            criteria: format!("user_id={bot_user_id}"),
        })
    }

    /// # One statement where Go builds eight
    ///
    /// Go assembles the `WHERE` from three optional conditions and adds a second `JOIN Users`
    /// only for `OnlyOrphaned`, so its statement has eight shapes. `query_as!` checks a literal,
    /// so the options are parameters here instead — and the two rewrites that makes necessary are
    /// both exact rather than approximate:
    ///
    /// - **`($1 OR b.deleteat = 0)`** and **`($2 = '' OR b.ownerid = $2)`** reproduce "the clause
    ///   is absent" as "the clause is satisfied", which is the same result set.
    /// - **`JOIN Users o` becomes `LEFT JOIN Users o` plus `o.id IS NOT NULL`.** An inner join is
    ///   a left join that also filters, and `Users.Id` is the primary key so at most one row
    ///   matches — the left join therefore adds no rows and duplicates none. Without the
    ///   `IS NOT NULL`, a bot whose `OwnerId` is a *plugin id* — which `com.mattermost.calls` is,
    ///   and it is the first row on this deployment — would survive the orphan filter that Go's
    ///   inner join drops it from.
    ///
    /// The predicates keep Go's bare `= 0` and `!= 0`, not a `COALESCE`, so a NULL `DeleteAt`
    /// fails them on both servers.
    #[tracing::instrument(skip_all, fields(owner_id = %options.owner_id, include_deleted = options.include_deleted, only_orphaned = options.only_orphaned, found))]
    async fn get_all(&self, options: &BotGetOptions) -> Result<BotList, StoreError> {
        let limit = i64::from(options.per_page);
        let offset = i64::from(options.page) * limit;

        let rows = sqlx::query_as!(
            BotRow,
            r#"
            SELECT b.userid                       AS "userid!",
                   COALESCE(u.username, '')       AS "username!",
                   COALESCE(u.firstname, '')      AS "displayname!",
                   COALESCE(b.description, '')    AS "description!",
                   COALESCE(b.ownerid, '')        AS "ownerid!",
                   COALESCE(b.lasticonupdate, 0)  AS "lasticonupdate!",
                   COALESCE(b.createat, 0)        AS "createat!",
                   COALESCE(b.updateat, 0)        AS "updateat!",
                   COALESCE(b.deleteat, 0)        AS "deleteat!"
              FROM bots b
              JOIN users u ON u.id = b.userid
              LEFT JOIN users o ON o.id = b.ownerid
             WHERE ($1 OR b.deleteat = 0)
               AND ($2 = '' OR b.ownerid = $2)
               AND (NOT $3 OR (o.id IS NOT NULL AND o.deleteat <> 0))
             ORDER BY b.createat ASC, u.username ASC
             LIMIT $4 OFFSET $5
            "#,
            options.include_deleted,
            options.owner_id,
            options.only_orphaned,
            limit,
            offset
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "error selecting all bots".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        Ok(BotList(rows.into_iter().map(Bot::from).collect()))
    }

    #[tracing::instrument(skip_all, fields(bot_user_id = %bot.user_id, owner_id = %bot.owner_id))]
    async fn save(&self, bot: &Bot) -> Result<Bot, StoreError> {
        // Go's `bot = bot.Clone()`: `PreSave` stamps the timestamps and lowercases the username,
        // and the caller's value must not see either — `App.CreateBot` still holds the bot it
        // built from the patch and returns the *store's* copy instead.
        let mut bot = bot.clone();
        bot.pre_save();
        if let Err(app_error) = bot.is_valid() {
            return Err(StoreError::Invalid {
                entity: "Bot",
                app_error,
            });
        }

        // Seven columns, Go's order. `Username` and `DisplayName` are **not** among them: they
        // live on `Users`, which the caller has already written.
        sqlx::query!(
            r#"
            INSERT INTO bots
                (userid, description, ownerid, lasticonupdate, createat, updateat, deleteat)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
            bot.user_id,
            bot.description,
            bot.owner_id,
            bot.last_icon_update,
            bot.create_at,
            bot.update_at,
            bot.delete_at,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("insert: user_id={}", bot.user_id),
            source,
        })?;

        Ok(bot)
    }

    #[tracing::instrument(skip_all, fields(bot_user_id = %bot.user_id, delete_at = bot.delete_at))]
    async fn update(&self, bot: &Bot) -> Result<Bot, StoreError> {
        let mut bot = bot.clone();
        bot.pre_update();
        if let Err(app_error) = bot.is_valid() {
            return Err(StoreError::Invalid {
                entity: "Bot",
                app_error,
            });
        }

        // `includeDeleted = true`, unconditionally — `UpdateBotActive` re-enabling a disabled bot
        // would otherwise never find the row it is about to clear `DeleteAt` on.
        let mut stored = self.get(&bot.user_id, true).await?;
        stored.description = bot.description;
        stored.owner_id = bot.owner_id;
        stored.last_icon_update = bot.last_icon_update;
        stored.update_at = bot.update_at;
        stored.delete_at = bot.delete_at;
        let bot = stored;

        let affected = sqlx::query!(
            r#"
            UPDATE bots
               SET description = $2, ownerid = $3, lasticonupdate = $4,
                   updateat = $5, deleteat = $6
             WHERE userid = $1
            "#,
            bot.user_id,
            bot.description,
            bot.owner_id,
            bot.last_icon_update,
            bot.update_at,
            bot.delete_at,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("update: user_id={}", bot.user_id),
            source,
        })?
        .rows_affected();

        // Go's guard is `count > 1`, not `!= 1`: `UserId` is the primary key so two rows cannot
        // match, and **zero** is not an error here because the `Get` above already proved the row
        // exists. Tightening it to `!= 1` would turn a concurrent delete into a 500 where Go
        // answers with the bot.
        if affected > 1 {
            return Err(StoreError::Db {
                context: format!(
                    "unexpected count while updating bot: count={affected}, userId={}",
                    bot.user_id
                ),
                source: sqlx::Error::RowNotFound,
            });
        }

        Ok(bot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The join's shape, stated where a reader will look for it: `display_name` is `Users.FirstName`
    /// and `username` is `Users.Username` — neither is a `Bots` column, and swapping them is a
    /// mutation the row mapping cannot catch on its own.
    #[test]
    fn a_row_maps_the_joined_user_columns_onto_the_bot() {
        let bot: Bot = BotRow {
            userid: "rcw3d9njxiy6pquw79ux5wqxjw".to_owned(),
            username: "calls".to_owned(),
            displayname: "Calls".to_owned(),
            description: "Calls Bot".to_owned(),
            ownerid: "com.mattermost.calls".to_owned(),
            lasticonupdate: 0,
            createat: 1788459398643,
            updateat: 1788459398644,
            deleteat: 0,
        }
        .into();

        assert_eq!(bot.username, "calls");
        assert_eq!(bot.display_name, "Calls");
        assert_eq!(bot.owner_id, "com.mattermost.calls");
        assert_eq!(bot.create_at, 1788459398643);
        assert_eq!(bot.update_at, 1788459398644);
    }
}
