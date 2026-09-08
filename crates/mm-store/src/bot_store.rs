//! Port of `SqlBotStore` (channels/store/sqlstore/bot_store.go), the two read methods.
//!
//! Ported for `getBot` (`GET /api/v4/bots/{bot_user_id}`) and `getBots` (`GET /api/v4/bots`). The
//! write half belongs to bot *creation*, which is a `POST` and not migrated.
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
