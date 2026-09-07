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

    /// Port of `SqlWebhookStore.GetIncoming` (webhook_store.go:128).
    ///
    /// Go's second parameter, `allowFromCache`, selects the cache layer's read
    /// (localcachelayer/webhook_layer.go:41) and has no meaning here — nothing on this side
    /// caches, so it is not in the signature. It is the only place either webhook route touches
    /// a Go cache at all: the list queries are uncached.
    fn get_incoming(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<IncomingWebhook, StoreError>> + Send;

    /// Port of `SqlWebhookStore.GetOutgoing` (webhook_store.go:263).
    /// Port of `SqlWebhookStore.SaveIncoming` (webhook_store.go:60).
    ///
    /// Go refuses a hook that already carries an id — `ErrInvalidInput`, which the app layer
    /// renders as `app.webhooks.save_incoming.existing.app_error` at **400**.
    fn save_incoming(
        &self,
        hook: &IncomingWebhook,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlWebhookStore.UpdateIncoming` (webhook_store.go:87).
    fn update_incoming(
        &self,
        hook: &IncomingWebhook,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlWebhookStore.DeleteIncoming` (webhook_store.go:151) — a **soft** delete.
    fn delete_incoming(
        &self,
        hook_id: &str,
        time: i64,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlWebhookStore.SaveOutgoing` (webhook_store.go:213).
    fn save_outgoing(
        &self,
        hook: &OutgoingWebhook,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlWebhookStore.UpdateOutgoing` (webhook_store.go:298).
    fn update_outgoing(
        &self,
        hook: &OutgoingWebhook,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlWebhookStore.DeleteOutgoing` (webhook_store.go:313) — a **soft** delete.
    fn delete_outgoing(
        &self,
        hook_id: &str,
        time: i64,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlWebhookStore.GetOutgoingByTeam` with `offset`/`limit` of `-1`
    /// (webhook_store.go:262), which is how both write paths ask for **every** hook on a team to
    /// run the trigger-word/callback intersection check.
    fn get_outgoing_by_team_unpaged(
        &self,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<OutgoingWebhook>, StoreError>> + Send;

    fn get_outgoing(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<OutgoingWebhook, StoreError>> + Send;
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

/// `OutgoingWebhooks.TriggerWhen` is an `integer` column and `OutgoingWebhook.trigger_when` is an
/// `i64`, matching Go's `int`.
fn trigger_when(value: i64) -> Result<i32, StoreError> {
    i32::try_from(value).map_err(|_| StoreError::Db {
        context: format!("trigger_when {value} does not fit the integer column"),
        source: sqlx::Error::Protocol("trigger_when out of range".to_owned()),
    })
}

/// The write half of [`string_array_column`]: `None` becomes SQL `NULL`, and anything else is
/// the JSON text Go's `StringArray.Value` produces.
fn string_array_text(value: Option<&StringArray>) -> Result<Option<String>, StoreError> {
    value
        .map(|value| {
            serde_json::to_string(value).map_err(|source| StoreError::Decode {
                entity: "Webhook",
                column: "string array",
                source,
            })
        })
        .transpose()
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
    #[tracing::instrument(skip_all, fields(id = %hook.id))]
    async fn save_incoming(&self, hook: &IncomingWebhook) -> Result<(), StoreError> {
        sqlx::query!(
            r#"
            INSERT INTO incomingwebhooks
                (id, createat, updateat, deleteat, userid, channelid, teamid, displayname,
                 description, username, iconurl, channellocked, lastused)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
            "#,
            hook.id,
            hook.create_at,
            hook.update_at,
            hook.delete_at,
            hook.user_id,
            hook.channel_id,
            hook.team_id,
            hook.display_name,
            hook.description,
            hook.username,
            hook.icon_url,
            hook.channel_locked,
            hook.last_used,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to save IncomingWebhook with id={}", hook.id),
            source,
        })?;
        Ok(())
    }

    /// **Ten columns, and `UserId` and `LastUsed` are not among them.** Go's `SET` list omits
    /// both, so an update cannot move a hook to a different creator and cannot rewrite its
    /// last-used stamp — which is why `UpdateIncomingWebhook` copies them off the old hook first
    /// and the store never has to.
    #[tracing::instrument(skip_all, fields(id = %hook.id))]
    async fn update_incoming(&self, hook: &IncomingWebhook) -> Result<(), StoreError> {
        sqlx::query!(
            r#"
            UPDATE incomingwebhooks
               SET createat = $2, updateat = $3, deleteat = $4, channelid = $5, teamid = $6,
                   displayname = $7, description = $8, username = $9, iconurl = $10,
                   channellocked = $11
             WHERE id = $1
            "#,
            hook.id,
            hook.create_at,
            hook.update_at,
            hook.delete_at,
            hook.channel_id,
            hook.team_id,
            hook.display_name,
            hook.description,
            hook.username,
            hook.icon_url,
            hook.channel_locked,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update IncomingWebhook with id={}", hook.id),
            source,
        })?;
        Ok(())
    }

    /// **`UpdateAt` takes the same value as `DeleteAt`**, not a separate `GetMillis()`. The two
    /// timestamps on a deleted hook are therefore always equal, which is observable through the
    /// list queries' ordering.
    #[tracing::instrument(skip(self), fields(id = %hook_id))]
    async fn delete_incoming(&self, hook_id: &str, time: i64) -> Result<(), StoreError> {
        sqlx::query!(
            "UPDATE incomingwebhooks SET deleteat = $1, updateat = $1 WHERE id = $2",
            time,
            hook_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update IncomingWebhook with id={hook_id}"),
            source,
        })?;
        Ok(())
    }

    /// The two array columns are written as **JSON text in a `varchar`**, matching how the read
    /// path decodes them — and a `None` becomes SQL `NULL`, which is the third state the model
    /// distinguishes from `[]`.
    #[tracing::instrument(skip_all, fields(id = %hook.id))]
    async fn save_outgoing(&self, hook: &OutgoingWebhook) -> Result<(), StoreError> {
        let trigger_words = string_array_text(hook.trigger_words.as_ref())?;
        let callback_urls = string_array_text(hook.callback_urls.as_ref())?;

        sqlx::query!(
            r#"
            INSERT INTO outgoingwebhooks
                (id, token, createat, updateat, deleteat, creatorid, channelid, teamid,
                 triggerwords, triggerwhen, callbackurls, displayname, description, contenttype,
                 username, iconurl)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
            "#,
            hook.id,
            hook.token,
            hook.create_at,
            hook.update_at,
            hook.delete_at,
            hook.creator_id,
            hook.channel_id,
            hook.team_id,
            trigger_words,
            // The column is `integer`; the model keeps Go's `int`, which is 64-bit on every
            // platform this runs on. `IsValid` caps the field at 1, so the narrowing cannot fail
            // for a hook that passed validation — but it is checked rather than cast, because a
            // silent wrap here would write a trigger rule nobody asked for.
            trigger_when(hook.trigger_when)?,
            callback_urls,
            hook.display_name,
            hook.description,
            hook.content_type,
            hook.username,
            hook.icon_url,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to save OutgoingWebhook with id={}", hook.id),
            source,
        })?;
        Ok(())
    }

    /// Unlike its incoming sibling, this `SET` list covers **every** column but `Id` — including
    /// `Token` and `CreatorId`. That is what lets `RegenOutgoingWebhookToken` reuse it with
    /// nothing changed but the token.
    #[tracing::instrument(skip_all, fields(id = %hook.id))]
    async fn update_outgoing(&self, hook: &OutgoingWebhook) -> Result<(), StoreError> {
        let trigger_words = string_array_text(hook.trigger_words.as_ref())?;
        let callback_urls = string_array_text(hook.callback_urls.as_ref())?;

        sqlx::query!(
            r#"
            UPDATE outgoingwebhooks
               SET createat = $2, updateat = $3, deleteat = $4, token = $5, creatorid = $6,
                   channelid = $7, teamid = $8, triggerwords = $9, triggerwhen = $10,
                   callbackurls = $11, displayname = $12, description = $13, contenttype = $14,
                   username = $15, iconurl = $16
             WHERE id = $1
            "#,
            hook.id,
            hook.create_at,
            hook.update_at,
            hook.delete_at,
            hook.token,
            hook.creator_id,
            hook.channel_id,
            hook.team_id,
            trigger_words,
            // The column is `integer`; the model keeps Go's `int`, which is 64-bit on every
            // platform this runs on. `IsValid` caps the field at 1, so the narrowing cannot fail
            // for a hook that passed validation — but it is checked rather than cast, because a
            // silent wrap here would write a trigger rule nobody asked for.
            trigger_when(hook.trigger_when)?,
            callback_urls,
            hook.display_name,
            hook.description,
            hook.content_type,
            hook.username,
            hook.icon_url,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update OutgoingWebhook with id={}", hook.id),
            source,
        })?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(id = %hook_id))]
    async fn delete_outgoing(&self, hook_id: &str, time: i64) -> Result<(), StoreError> {
        sqlx::query!(
            "UPDATE outgoingwebhooks SET deleteat = $1, updateat = $1 WHERE id = $2",
            time,
            hook_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update OutgoingWebhook with id={hook_id}"),
            source,
        })?;
        Ok(())
    }

    /// **No `DeleteAt` predicate**, and that is Go's. The intersection check therefore compares a
    /// new hook against *deleted* ones too, so a trigger word freed by deleting a hook stays
    /// unusable. Reproduced: the alternative accepts hooks Go refuses.
    ///
    /// Go's `-1` offset and limit reach `squirrel` as `OFFSET -1 LIMIT -1`, which Postgres treats
    /// as "no offset, no limit"; expressed here as the absence of both clauses.
    #[tracing::instrument(skip(self), fields(team_id = %team_id, found))]
    async fn get_outgoing_by_team_unpaged(
        &self,
        team_id: &str,
    ) -> Result<Vec<OutgoingWebhook>, StoreError> {
        let rows = sqlx::query_as!(
            OutgoingWebhookRow,
            r#"
            SELECT id            AS "id!",
                   token         AS "token!",
                   createat      AS "createat!",
                   updateat      AS "updateat!",
                   deleteat      AS "deleteat!",
                   creatorid     AS "creatorid!",
                   channelid     AS "channelid!",
                   teamid        AS "teamid!",
                   triggerwords  AS "triggerwords?",
                   triggerwhen   AS "triggerwhen!",
                   callbackurls  AS "callbackurls?",
                   displayname   AS "displayname!",
                   description   AS "description!",
                   contenttype   AS "contenttype!",
                   username      AS "username!",
                   iconurl       AS "iconurl!"
              FROM outgoingwebhooks
             WHERE teamid = $1
            "#,
            team_id,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get OutgoingWebhooks with teamId={team_id}"),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        rows.into_iter()
            .map(OutgoingWebhookRow::into_model)
            .collect()
    }

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

    /// # A soft-deleted hook is a **404**, not a row with `delete_at` set
    ///
    /// `DeleteAt = 0` is in the `WHERE` (webhook_store.go:134), so deleting a hook makes this
    /// route stop finding it rather than return it deleted — the same predicate the list queries
    /// carry, reaching a different answer because this one is a single-row `Get`.
    #[tracing::instrument(skip_all, fields(id, found))]
    async fn get_incoming(&self, id: &str) -> Result<IncomingWebhook, StoreError> {
        let row = sqlx::query_as!(
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
             WHERE id = $1
               AND deleteat = 0
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get IncomingWebhook with id={id}"),
            source,
        })?
        .ok_or_else(|| StoreError::NotFound {
            entity: "IncomingWebhook",
            criteria: format!("id={id}"),
        })?;

        tracing::Span::current().record("found", true);
        Ok(IncomingWebhook::from(row))
    }

    #[tracing::instrument(skip_all, fields(id, found))]
    async fn get_outgoing(&self, id: &str) -> Result<OutgoingWebhook, StoreError> {
        let row = sqlx::query_as!(
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
             WHERE id = $1
               AND deleteat = 0
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get OutgoingWebhook with id={id}"),
            source,
        })?
        .ok_or_else(|| StoreError::NotFound {
            entity: "OutgoingWebhook",
            criteria: format!("id={id}"),
        })?;

        tracing::Span::current().record("found", true);
        row.into_model()
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
