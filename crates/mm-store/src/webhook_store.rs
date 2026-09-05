//! Port of `SqlWebhookStore` (channels/store/sqlstore/webhook_store.go) — the reads
//! `getIncomingHooks` and `getOutgoingHooks` need, and nothing else.
//!
//! The single-hook reads and every write are not ported: those two list routes are all that is
//! migrated over this table, and a store function with no caller is a guess about a query nothing
//! can falsify.

use mm_model::incoming_webhook::IncomingWebhook;
use mm_model::outgoing_webhook::OutgoingWebhook;
use mm_model::utils::StringArray;
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

    /// Port of `SqlWebhookStore.GetOutgoingListByUser` (webhook_store.go:283) — every team.
    fn get_outgoing_list_by_user(
        &self,
        user_id: &str,
        offset: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<OutgoingWebhook>, StoreError>> + Send;

    /// Port of `SqlWebhookStore.GetOutgoingByChannelByUser` (webhook_store.go:309).
    fn get_outgoing_by_channel_by_user(
        &self,
        channel_id: &str,
        user_id: &str,
        offset: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<OutgoingWebhook>, StoreError>> + Send;

    /// Port of `SqlWebhookStore.GetOutgoingByTeamByUser` (webhook_store.go:337).
    fn get_outgoing_by_team_by_user(
        &self,
        team_id: &str,
        user_id: &str,
        offset: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<OutgoingWebhook>, StoreError>> + Send;
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

/// One row of `outgoingWebhookSelectQuery` (webhook_store.go:54-73).
///
/// `TriggerWords` and `CallbackURLs` are `model.StringArray`, which the schema stores as a
/// **JSON array inside a `varchar`** — `StringArray.Scan` (model/utils.go:118) `json.Unmarshal`s
/// whatever the column holds and leaves the field **nil** for a SQL NULL. So all three states are
/// distinguishable on the wire: `null`, `[]`, and a populated array. That is why the model's field
/// is an `Option` and why this row reads the column as an `Option<String>` rather than defaulting
/// it to `[]`.
struct OutgoingWebhookRow {
    id: String,
    token: String,
    createat: i64,
    updateat: i64,
    deleteat: i64,
    creatorid: String,
    channelid: String,
    teamid: String,
    triggerwords: Option<String>,
    triggerwhen: i32,
    callbackurls: Option<String>,
    displayname: String,
    description: String,
    contenttype: String,
    username: String,
    iconurl: String,
}

/// `StringArray.Scan`: NULL leaves the field nil; anything else is `json.Unmarshal`ed, and a
/// failure there fails the whole query on Go's side too.
///
/// An **empty string** is therefore a scan error, not an empty array — `json.Unmarshal([]byte(""))`
/// is "unexpected end of JSON input". Reproduced rather than smoothed into `[]`: a column Go
/// cannot read is a row neither server can serve, and silently inventing a value here would make
/// us answer where Go 500s.
fn string_array_column(
    raw: Option<String>,
    column: &'static str,
) -> Result<Option<StringArray>, StoreError> {
    match raw {
        None => Ok(None),
        Some(json) => serde_json::from_str(&json)
            .map(Some)
            .map_err(|source| StoreError::Decode {
                entity: "OutgoingWebhook",
                column,
                source,
            }),
    }
}

impl OutgoingWebhookRow {
    fn into_model(self) -> Result<OutgoingWebhook, StoreError> {
        Ok(OutgoingWebhook {
            id: self.id,
            token: self.token,
            create_at: self.createat,
            update_at: self.updateat,
            delete_at: self.deleteat,
            creator_id: self.creatorid,
            channel_id: self.channelid,
            team_id: self.teamid,
            trigger_words: string_array_column(self.triggerwords, "TriggerWords")?,
            // Go's field is `int`; the column is a 4-byte `integer`. The widening is ours and
            // changes nothing — `TriggerWhen` holds 0 or 1 (`TRIGGER_WORDS_*`).
            trigger_when: i64::from(self.triggerwhen),
            callback_urls: string_array_column(self.callbackurls, "CallbackURLs")?,
            display_name: self.displayname,
            description: self.description,
            content_type: self.contenttype,
            username: self.username,
            icon_url: self.iconurl,
        })
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

    /// # The user filter is `CreatorId`, not `UserId`
    ///
    /// The incoming table names its owner column `UserId` and this one names it `CreatorId`
    /// (webhook_store.go:295). The two routes read the same-looking predicate off different
    /// columns, and the mistake would be invisible on a fixture where the creator is also the
    /// only user.
    #[tracing::instrument(skip_all, fields(user_id, offset, limit, found))]
    async fn get_outgoing_list_by_user(
        &self,
        user_id: &str,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<OutgoingWebhook>, StoreError> {
        let rows = sqlx::query_as!(
            OutgoingWebhookRow,
            r#"
            SELECT id                          AS "id!",
                   COALESCE(token, '')         AS "token!",
                   COALESCE(createat, 0)       AS "createat!",
                   COALESCE(updateat, 0)       AS "updateat!",
                   COALESCE(deleteat, 0)       AS "deleteat!",
                   COALESCE(creatorid, '')     AS "creatorid!",
                   COALESCE(channelid, '')     AS "channelid!",
                   COALESCE(teamid, '')        AS "teamid!",
                   triggerwords                AS "triggerwords?",
                   COALESCE(triggerwhen, 0)    AS "triggerwhen!",
                   callbackurls                AS "callbackurls?",
                   COALESCE(displayname, '')   AS "displayname!",
                   COALESCE(description, '')   AS "description!",
                   COALESCE(contenttype, '')   AS "contenttype!",
                   COALESCE(username, '')      AS "username!",
                   COALESCE(iconurl, '')       AS "iconurl!"
              FROM outgoingwebhooks
             WHERE deleteat = 0
               AND ($1 = '' OR creatorid = $1)
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
            context: "failed to find OutgoingWebhooks".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        rows.into_iter()
            .map(OutgoingWebhookRow::into_model)
            .collect()
    }

    /// # `LIMIT`/`OFFSET` are conditional in Go here, and unconditional in the list query
    ///
    /// `GetOutgoingByChannelByUser` applies them only `if limit >= 0 && offset >= 0`
    /// (webhook_store.go:322) — a guard `GetOutgoingListByUser` does not have. Through this route
    /// both come from `web.ParamsFromRequest`, which floors `page` at 0 and `per_page` at 0, so
    /// the guard is always true and the branch is unreachable. It is **not** reproduced: a
    /// condition that cannot be false is not a behaviour, and writing it would invite a reader to
    /// find the fixture that exercises it. Recorded here instead.
    #[tracing::instrument(skip_all, fields(channel_id, user_id, offset, limit, found))]
    async fn get_outgoing_by_channel_by_user(
        &self,
        channel_id: &str,
        user_id: &str,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<OutgoingWebhook>, StoreError> {
        let rows = sqlx::query_as!(
            OutgoingWebhookRow,
            r#"
            SELECT id                          AS "id!",
                   COALESCE(token, '')         AS "token!",
                   COALESCE(createat, 0)       AS "createat!",
                   COALESCE(updateat, 0)       AS "updateat!",
                   COALESCE(deleteat, 0)       AS "deleteat!",
                   COALESCE(creatorid, '')     AS "creatorid!",
                   COALESCE(channelid, '')     AS "channelid!",
                   COALESCE(teamid, '')        AS "teamid!",
                   triggerwords                AS "triggerwords?",
                   COALESCE(triggerwhen, 0)    AS "triggerwhen!",
                   callbackurls                AS "callbackurls?",
                   COALESCE(displayname, '')   AS "displayname!",
                   COALESCE(description, '')   AS "description!",
                   COALESCE(contenttype, '')   AS "contenttype!",
                   COALESCE(username, '')      AS "username!",
                   COALESCE(iconurl, '')       AS "iconurl!"
              FROM outgoingwebhooks
             WHERE channelid = $1
               AND deleteat = 0
               AND ($2 = '' OR creatorid = $2)
             ORDER BY displayname, id
             LIMIT $3 OFFSET $4
            "#,
            channel_id,
            user_id,
            limit,
            offset
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find OutgoingWebhooks".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        rows.into_iter()
            .map(OutgoingWebhookRow::into_model)
            .collect()
    }

    #[tracing::instrument(skip_all, fields(team_id, user_id, offset, limit, found))]
    async fn get_outgoing_by_team_by_user(
        &self,
        team_id: &str,
        user_id: &str,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<OutgoingWebhook>, StoreError> {
        let rows = sqlx::query_as!(
            OutgoingWebhookRow,
            r#"
            SELECT id                          AS "id!",
                   COALESCE(token, '')         AS "token!",
                   COALESCE(createat, 0)       AS "createat!",
                   COALESCE(updateat, 0)       AS "updateat!",
                   COALESCE(deleteat, 0)       AS "deleteat!",
                   COALESCE(creatorid, '')     AS "creatorid!",
                   COALESCE(channelid, '')     AS "channelid!",
                   COALESCE(teamid, '')        AS "teamid!",
                   triggerwords                AS "triggerwords?",
                   COALESCE(triggerwhen, 0)    AS "triggerwhen!",
                   callbackurls                AS "callbackurls?",
                   COALESCE(displayname, '')   AS "displayname!",
                   COALESCE(description, '')   AS "description!",
                   COALESCE(contenttype, '')   AS "contenttype!",
                   COALESCE(username, '')      AS "username!",
                   COALESCE(iconurl, '')       AS "iconurl!"
              FROM outgoingwebhooks
             WHERE teamid = $1
               AND deleteat = 0
               AND ($2 = '' OR creatorid = $2)
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
            context: "failed to find OutgoingWebhooks".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        rows.into_iter()
            .map(OutgoingWebhookRow::into_model)
            .collect()
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
