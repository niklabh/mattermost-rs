//! Port of the read side of `SqlDraftStore` (channels/store/sqlstore/draft_store.go).
//!
//! One query, behind `GET /api/v4/users/{user_id}/teams/{team_id}/drafts`.

use mm_model::draft::Draft;
use mm_model::utils::{StringArray, StringInterface};
use sqlx::PgPool;

use crate::error::StoreError;

/// Port of `store.DraftStore`, narrowed to what the three drafts routes need.
pub trait DraftStore {
    /// Port of `SqlDraftStore.GetDraftsForUser` (draft_store.go:125).
    fn get_drafts_for_user(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<Draft>, StoreError>> + Send;

    /// Port of `SqlDraftStore.Get` (draft_store.go:73).
    ///
    /// `include_deleted` drops the `DeleteAt = 0` predicate. `Ok(None)` is Go's
    /// `store.NewErrNotFound("Draft", channelId)` — note it names the **channel**, not the draft.
    fn get(
        &self,
        user_id: &str,
        channel_id: &str,
        root_id: &str,
        include_deleted: bool,
    ) -> impl std::future::Future<Output = Result<Option<Draft>, StoreError>> + Send;

    /// Port of `SqlDraftStore.Upsert` (draft_store.go:100).
    fn upsert(
        &self,
        draft: &Draft,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlDraftStore.Delete` (draft_store.go:168) — a **hard** delete.
    fn delete(
        &self,
        user_id: &str,
        channel_id: &str,
        root_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlDraftStore.determineMaxDraftSize` (draft_store.go:236).
    ///
    /// **A deployment artifact, not a constant.** Go reads `character_maximum_length` for
    /// `Drafts.Message` out of `information_schema` and divides by four — "assume a worst-case
    /// representation of four bytes per rune" — so the limit depends on whether the column was
    /// ever widened. It is 65535/4 = 16383 on the development stack and 4000/4 = 1000 on a server
    /// that never migrated. Hard-coding either would refuse messages Go accepts, or accept ones
    /// it refuses.
    ///
    /// Go memoises it in a `sync.Once`; the caching is left to the caller here.
    fn max_draft_size(&self) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;
}

#[derive(Debug, Clone)]
pub struct SqlDraftStore {
    pool: PgPool,
}

impl SqlDraftStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl DraftStore for SqlDraftStore {
    /// The **select** list carries `COALESCE(Type, '')` while the *insert* writes `Type` raw —
    /// `draftSelectColumns` rewrites only the last entry of `draftSliceColumns`
    /// (draft_store.go:42). Reproduced on both sides.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, channel_id = %channel_id, found))]
    async fn get(
        &self,
        user_id: &str,
        channel_id: &str,
        root_id: &str,
        include_deleted: bool,
    ) -> Result<Option<Draft>, StoreError> {
        // Go builds the `DeleteAt` predicate conditionally; expressed inside one static statement
        // so sqlx keeps checking it at compile time.
        let row = sqlx::query!(
            r#"
            SELECT createat            AS "create_at!",
                   updateat            AS "update_at!",
                   deleteat            AS "delete_at!",
                   message             AS "message!",
                   rootid              AS "root_id!",
                   channelid           AS "channel_id!",
                   userid              AS "user_id!",
                   fileids             AS "file_ids?",
                   props               AS "props?",
                   priority            AS "priority?",
                   COALESCE(type, '')  AS "draft_type!"
              FROM drafts
             WHERE userid = $1 AND channelid = $2 AND rootid = $3
               AND ($4 OR deleteat = 0)
            "#,
            user_id,
            channel_id,
            root_id,
            include_deleted,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find draft with channelid = {channel_id}"),
            source,
        })?;

        tracing::Span::current().record("found", row.is_some());

        row.map(|row| {
            Ok(Draft {
                create_at: row.create_at,
                update_at: row.update_at,
                delete_at: row.delete_at,
                user_id: row.user_id,
                channel_id: row.channel_id,
                root_id: row.root_id,
                message: row.message,
                draft_type: row.draft_type,
                props: decode_map("props", row.props)?,
                file_ids: decode_array("fileids", row.file_ids)?,
                metadata: None,
                priority: decode_map("priority", row.priority)?,
            })
        })
        .transpose()
    }

    /// # The conflict clause updates seven columns and **not** `CreateAt`
    ///
    /// `ON CONFLICT (UserId, ChannelId, RootId) DO UPDATE SET UpdateAt, Message, Props, FileIds,
    /// Priority, Type, DeleteAt = 0`. So editing a draft keeps its original creation time, and
    /// the `DeleteAt = 0` in the clause is what revives a draft that was soft-deleted by an older
    /// server — the delete path is a hard DELETE now, so nothing this port writes can be revived,
    /// but rows written before that change still can.
    ///
    /// The three JSON columns are `varchar` holding JSON text, so they are serialised here rather
    /// than bound as `jsonb`. **`FileIds` and the two maps are `null` when absent, not `'{}'`** —
    /// Go's `ArrayToJSON`/`StringInterfaceToJSON` render a nil as `null`, and the read path's
    /// decoder treats a NULL column and the four bytes `null` alike.
    #[tracing::instrument(skip(self, draft), fields(user_id = %draft.user_id, channel_id = %draft.channel_id))]
    async fn upsert(&self, draft: &Draft) -> Result<(), StoreError> {
        let file_ids = json_text(draft.file_ids.as_ref())?;
        let props = json_text(draft.props.as_ref())?;
        let priority = json_text(draft.priority.as_ref())?;

        sqlx::query!(
            r#"
            INSERT INTO drafts
                (createat, updateat, deleteat, message, rootid, channelid, userid,
                 fileids, props, priority, type)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            ON CONFLICT (userid, channelid, rootid)
                DO UPDATE SET updateat = $2,
                              message  = $4,
                              props    = $9,
                              fileids  = $8,
                              priority = $10,
                              type     = $11,
                              deleteat = 0
            "#,
            draft.create_at,
            draft.update_at,
            draft.delete_at,
            draft.message,
            draft.root_id,
            draft.channel_id,
            draft.user_id,
            file_ids,
            props,
            priority,
            draft.draft_type,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to upsert Draft".to_owned(),
            source,
        })?;

        Ok(())
    }

    /// **A hard DELETE.** `Drafts.DeleteAt` still exists and the read path still filters on it,
    /// but Go's comment on the model field says "we now just hard delete the rows" — so a
    /// withdrawn draft leaves nothing behind, unlike a withdrawn reaction.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, channel_id = %channel_id))]
    async fn delete(
        &self,
        user_id: &str,
        channel_id: &str,
        root_id: &str,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            "DELETE FROM drafts WHERE userid = $1 AND channelid = $2 AND rootid = $3",
            user_id,
            channel_id,
            root_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to delete Draft".to_owned(),
            source,
        })?;

        Ok(())
    }

    #[tracing::instrument(skip(self), fields(max))]
    async fn max_draft_size(&self) -> Result<i64, StoreError> {
        let bytes: i64 = sqlx::query_scalar!(
            r#"
            SELECT COALESCE(character_maximum_length, 0)::bigint AS "length!"
              FROM information_schema.columns
             WHERE table_name = 'drafts' AND column_name = 'message'
            "#,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "Unable to determine the maximum supported draft size".to_owned(),
            source,
        })?
        // Go logs a warning and carries on with a **zero** `maxDraftSizeBytes` when the query
        // fails, which makes every non-empty draft invalid. The same value is used here rather
        // than a friendlier default, because the alternative is accepting drafts Go refuses.
        .unwrap_or(0);

        let max = bytes / 4;
        tracing::Span::current().record("max", max);
        Ok(max)
    }

    /// # The channel-members join is the access check
    ///
    /// There is no permission check anywhere above this for the *channels* the drafts are in —
    /// `getDrafts` checks `view_team` on the team and nothing else. A draft in a channel the user
    /// has since left is excluded here, by the inner join on `ChannelMembers`, and that join is
    /// the only thing standing between a client and a message it wrote in a channel it no longer
    /// belongs to.
    ///
    /// # The team filter admits the empty team
    ///
    /// `Channels.TeamId = $2 OR Channels.TeamId = ''` — a DM or group message has no team, so it
    /// belongs to *every* team's draft list. Dropping the second half hides every DM draft; and
    /// the column is `NOT NULL DEFAULT ''` here, so the comparison is against `''` rather than a
    /// `COALESCE`.
    ///
    /// # Go's un-teamed branch is not ported
    ///
    /// `GetDraftsForUser` skips the `Channels` join entirely when `teamID` is empty. The only
    /// caller this port serves is the api4 route, whose `RequireTeamId` has already refused an
    /// empty or malformed segment, so that branch is unreachable from the wire. Adding it would
    /// mean a second query for a case nothing can produce.
    ///
    /// # Types, props and the order
    ///
    /// `Props`, `FileIds` and `Priority` are **`varchar` columns holding JSON text**, not `jsonb`
    /// — the same shape `Posts.FileIds` has and the opposite of `Posts.Props`, so the parse is
    /// ours to do. `Type` is the one column Go coalesces: it was added by a later migration and
    /// older rows hold NULL.
    ///
    /// `ORDER BY UpdateAt DESC` carries no tiebreak, in Go either. Two drafts saved in the same
    /// millisecond are returned in an unspecified order by both servers.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, team_id = %team_id, found))]
    async fn get_drafts_for_user(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> Result<Vec<Draft>, StoreError> {
        let rows = sqlx::query!(
            r#"
            SELECT d.createat            AS "create_at!",
                   d.updateat            AS "update_at!",
                   d.message             AS "message!",
                   d.rootid              AS "root_id!",
                   d.channelid           AS "channel_id!",
                   d.userid              AS "user_id!",
                   d.fileids             AS "file_ids?",
                   d.props               AS "props?",
                   d.priority            AS "priority?",
                   COALESCE(d.type, '')  AS "draft_type!"
              FROM drafts d
              INNER JOIN channelmembers cm ON cm.channelid = d.channelid
              INNER JOIN channels c ON c.id = d.channelid
             WHERE d.deleteat = 0
               AND d.userid = $1
               AND cm.userid = $1
               AND (c.teamid = $2 OR c.teamid = '')
             ORDER BY d.updateat DESC
            "#,
            user_id,
            team_id,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to get user drafts".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());

        rows.into_iter()
            .map(|row| {
                Ok(Draft {
                    create_at: row.create_at,
                    update_at: row.update_at,
                    // Never selected, so Go's struct keeps its zero — `"delete_at": 0` on the
                    // wire even though the predicate above guarantees it.
                    delete_at: 0,
                    user_id: row.user_id,
                    channel_id: row.channel_id,
                    root_id: row.root_id,
                    message: row.message,
                    draft_type: row.draft_type,
                    props: decode_map("props", row.props)?,
                    file_ids: decode_array("fileids", row.file_ids)?,
                    // Filled in by the app layer, from the file ids above.
                    metadata: None,
                    priority: decode_map("priority", row.priority)?,
                })
            })
            .collect()
    }
}

/// The write half of the read path's decoders: a `None` becomes the four bytes `null`, matching
/// Go's `ArrayToJSON(nil)` and `StringInterfaceToJSON(nil)`.
fn json_text<T: serde::Serialize>(value: Option<&T>) -> Result<String, StoreError> {
    match value {
        Some(value) => serde_json::to_string(value).map_err(|source| StoreError::Decode {
            entity: "Draft",
            column: "json column",
            source,
        }),
        None => Ok("null".to_owned()),
    }
}

/// `StringArray.Scan` (model/utils.go:118): NULL stays nil, anything else is parsed as JSON.
fn decode_array(
    column: &'static str,
    raw: Option<String>,
) -> Result<Option<StringArray>, StoreError> {
    raw.map(|raw| serde_json::from_str::<StringArray>(&raw))
        .transpose()
        .map_err(|source| StoreError::Decode {
            entity: "Draft",
            column,
            source,
        })
}

/// `StringInterface.Scan` (model/utils.go:143), which is **not** the mirror of [`decode_array`].
///
/// Go's scanner is handed a freshly made empty map and returns without touching it on NULL, so a
/// NULL column is an *empty map*, not nil. Only the JSON text `null` unmarshals the map back to
/// nil. `Draft.Props` has no `omitempty`, so the two are distinguishable on the wire: `{}` versus
/// `null`.
fn decode_map(
    column: &'static str,
    raw: Option<String>,
) -> Result<Option<StringInterface>, StoreError> {
    let Some(raw) = raw else {
        return Ok(Some(StringInterface::new()));
    };
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|source| StoreError::Decode {
            entity: "Draft",
            column,
            source,
        })?;
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Object(map) => Ok(Some(StringInterface::from_iter(map))),
        other => Err(StoreError::Decode {
            entity: "Draft",
            column,
            source: serde::de::Error::custom(format!(
                "{column} is a {}, not an object",
                match other {
                    serde_json::Value::Array(_) => "array",
                    serde_json::Value::String(_) => "string",
                    serde_json::Value::Number(_) => "number",
                    _ => "boolean",
                }
            )),
        }),
    }
}
