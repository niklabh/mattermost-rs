//! Port of `SqlWebhookStore` (channels/store/sqlstore/webhook_store.go) — the three incoming-hook
//! reads `getIncomingHooks` needs, and nothing else.
//!
//! The outgoing half, the single-hook reads and every write are not ported: `GET /api/v4/hooks/
//! incoming` is the only route migrated over this table, and a store function with no caller is a
//! guess about a query nothing can falsify.

use mm_model::incoming_webhook::IncomingWebhook;
use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `store.WebhookStore` (store/store.go) that is ported.
pub trait WebhookStore {
    /// Port of `SqlWebhookStore.GetIncomingListByUser` (webhook_store.go:178) — every team.
    fn get_incoming_list_by_user(
        &self,
        user_id: &str,
        offset: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<IncomingWebhook>, StoreError>> + Send;

    /// Port of `SqlWebhookStore.GetIncomingByTeamByUser` (webhook_store.go:198) — one team.
    fn get_incoming_by_team_by_user(
        &self,
        team_id: &str,
        user_id: &str,
        offset: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<IncomingWebhook>, StoreError>> + Send;

    /// Port of `SqlWebhookStore.AnalyticsIncomingCount` (webhook_store.go:409).
    fn analytics_incoming_count(
        &self,
        team_id: &str,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlWebhookStore {
    pool: PgPool,
}

impl SqlWebhookStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// One row of `incomingWebhookSelectQuery` (webhook_store.go:35-51).
///
/// `LastUsed` is the only column the schema declares `NOT NULL`; the rest are nullable while Go
/// scans them into plain `string`/`int64`/`bool`, so a NULL fails Go's own scan. `COALESCE` gives
/// the zero value Go's model would have held, which keeps a hand-written row readable rather than
/// taking the whole query down.
struct IncomingWebhookRow {
    id: String,
    createat: i64,
    updateat: i64,
    deleteat: i64,
    userid: String,
    channelid: String,
    teamid: String,
    displayname: String,
    description: String,
    username: String,
    iconurl: String,
    channellocked: bool,
    lastused: i64,
}

impl From<IncomingWebhookRow> for IncomingWebhook {
    fn from(row: IncomingWebhookRow) -> Self {
        IncomingWebhook {
            id: row.id,
            create_at: row.createat,
            update_at: row.updateat,
            delete_at: row.deleteat,
            user_id: row.userid,
            channel_id: row.channelid,
            team_id: row.teamid,
            display_name: row.displayname,
            description: row.description,
            username: row.username,
            icon_url: row.iconurl,
            channel_locked: row.channellocked,
            last_used: row.lastused,
        }
    }
}

impl WebhookStore for SqlWebhookStore {
    /// # `ORDER BY DisplayName, Id`
    ///
    /// Two keys, and the second is what makes the page stable: display names are not unique —
    /// nothing validates them for uniqueness — so `Id` is the tiebreak that keeps `OFFSET`
    /// paging from repeating or skipping a row. Both are reproduced; dropping `Id` would look
    /// harmless and produce a page that changes between two identical requests.
    ///
    /// # The user filter is conditional, and the *caller* decides
    ///
    /// An empty `user_id` means "every user's hooks", which is what the handler passes once it
    /// has confirmed `manage_others_incoming_webhooks`. Unlike the audit store — where an empty
    /// id was unreachable and narrowing it was the safe direction — here the empty case is the
    /// **normal** one for an admin, so the branch is reproduced as a branch.
    #[tracing::instrument(skip_all, fields(user_id, offset, limit, found))]
    async fn get_incoming_list_by_user(
        &self,
        user_id: &str,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<IncomingWebhook>, StoreError> {
        let rows = sqlx::query_as!(
            IncomingWebhookRow,
            r#"
            SELECT id                          AS "id!",
                   COALESCE(createat, 0)       AS "createat!",
                   COALESCE(updateat, 0)       AS "updateat!",
                   COALESCE(deleteat, 0)       AS "deleteat!",
                   COALESCE(userid, '')        AS "userid!",
                   COALESCE(channelid, '')     AS "channelid!",
                   COALESCE(teamid, '')        AS "teamid!",
                   COALESCE(displayname, '')   AS "displayname!",
                   COALESCE(description, '')   AS "description!",
                   COALESCE(username, '')      AS "username!",
                   COALESCE(iconurl, '')       AS "iconurl!",
                   COALESCE(channellocked, FALSE) AS "channellocked!",
                   lastused                    AS "lastused!"
              FROM incomingwebhooks
             WHERE deleteat = 0
               AND ($1 = '' OR userid = $1)
             ORDER BY displayname, id
             LIMIT $2 OFFSET $3
            "#,
            user_id,
            limit,
            offset
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find IncomingWebhooks".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        Ok(rows.into_iter().map(IncomingWebhook::from).collect())
    }

    /// The same query with `TeamId` added. Go builds it from the same builder and adds one
    /// `sq.Eq`; the team predicate is unconditional there, so an empty `team_id` here matches
    /// nothing rather than everything — and the handler never passes one, because an empty
    /// `team_id` takes the other branch entirely.
    #[tracing::instrument(skip_all, fields(team_id, user_id, offset, limit, found))]
    async fn get_incoming_by_team_by_user(
        &self,
        team_id: &str,
        user_id: &str,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<IncomingWebhook>, StoreError> {
        let rows = sqlx::query_as!(
            IncomingWebhookRow,
            r#"
            SELECT id                          AS "id!",
                   COALESCE(createat, 0)       AS "createat!",
                   COALESCE(updateat, 0)       AS "updateat!",
                   COALESCE(deleteat, 0)       AS "deleteat!",
                   COALESCE(userid, '')        AS "userid!",
                   COALESCE(channelid, '')     AS "channelid!",
                   COALESCE(teamid, '')        AS "teamid!",
                   COALESCE(displayname, '')   AS "displayname!",
                   COALESCE(description, '')   AS "description!",
                   COALESCE(username, '')      AS "username!",
                   COALESCE(iconurl, '')       AS "iconurl!",
                   COALESCE(channellocked, FALSE) AS "channellocked!",
                   lastused                    AS "lastused!"
              FROM incomingwebhooks
             WHERE teamid = $1
               AND deleteat = 0
               AND ($2 = '' OR userid = $2)
             ORDER BY displayname, id
             LIMIT $3 OFFSET $4
            "#,
            team_id,
            user_id,
            limit,
            offset
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find IncomingWebhook with teamId={team_id}"),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        Ok(rows.into_iter().map(IncomingWebhook::from).collect())
    }

    /// # Both filters are conditional here, unlike in the list queries
    ///
    /// `AnalyticsIncomingCount` adds `TeamId` only when non-empty (webhook_store.go:416), so the
    /// **count is over every team** when the caller asked for no team — while
    /// `get_incoming_by_team_by_user` above always has a team. That asymmetry is Go's, and it is
    /// the one thing about `include_total_count` a reader would get wrong: the count and the page
    /// answer different questions when `team_id` is absent, and they still ship in the same
    /// object.
    #[tracing::instrument(skip_all, fields(team_id, user_id, count))]
    async fn analytics_incoming_count(
        &self,
        team_id: &str,
        user_id: &str,
    ) -> Result<i64, StoreError> {
        let count = sqlx::query_scalar!(
            r#"
            SELECT COUNT(*) AS "count!"
              FROM incomingwebhooks
             WHERE deleteat = 0
               AND ($1 = '' OR teamid = $1)
               AND ($2 = '' OR userid = $2)
            "#,
            team_id,
            user_id
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to count IncomingWebhooks".to_owned(),
            source,
        })?;

        tracing::Span::current().record("count", count);
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every persisted column lands on its field. Thirteen columns and thirteen fields, with two
    /// pairs (`display_name`/`description`, `username`/`icon_url`) that are adjacent strings and
    /// would swap silently.
    #[test]
    fn a_row_maps_onto_the_model() {
        let hook = IncomingWebhook::from(IncomingWebhookRow {
            id: "nnk54zogfbrxxeepga9g6m6c5e".to_owned(),
            createat: 1_788_636_490_668,
            updateat: 1_788_636_490_669,
            deleteat: 1_788_636_490_670,
            userid: "6rtg4qbe5bn55mw5t6gphxyaxa".to_owned(),
            channelid: "ezytoszqbfg4i8tofm8zis3apc".to_owned(),
            teamid: "tnewcuy4ztgw5doi7j5ytqxg9w".to_owned(),
            displayname: "mmrs hook".to_owned(),
            description: "a description".to_owned(),
            username: "hookbot".to_owned(),
            iconurl: "http://example.invalid/i.png".to_owned(),
            channellocked: true,
            lastused: 1_788_636_490_671,
        });

        assert_eq!(hook.id, "nnk54zogfbrxxeepga9g6m6c5e");
        assert_eq!(hook.create_at, 1_788_636_490_668);
        assert_eq!(hook.update_at, 1_788_636_490_669);
        assert_eq!(hook.delete_at, 1_788_636_490_670);
        assert_eq!(hook.user_id, "6rtg4qbe5bn55mw5t6gphxyaxa");
        assert_eq!(hook.channel_id, "ezytoszqbfg4i8tofm8zis3apc");
        assert_eq!(hook.team_id, "tnewcuy4ztgw5doi7j5ytqxg9w");
        assert_eq!(hook.display_name, "mmrs hook");
        assert_eq!(hook.description, "a description");
        assert_eq!(hook.username, "hookbot");
        assert_eq!(hook.icon_url, "http://example.invalid/i.png");
        assert!(hook.channel_locked);
        assert_eq!(hook.last_used, 1_788_636_490_671);
    }
}
