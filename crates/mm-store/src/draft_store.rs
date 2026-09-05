//! Port of the read side of `SqlDraftStore` (channels/store/sqlstore/draft_store.go).
//!
//! One query, behind `GET /api/v4/users/{user_id}/teams/{team_id}/drafts`.

use mm_model::draft::Draft;
use mm_model::utils::{StringArray, StringInterface};
use sqlx::PgPool;

use crate::error::StoreError;

/// Port of `store.DraftStore`, narrowed to the one read the drafts route makes.
pub trait DraftStore {
    /// Port of `SqlDraftStore.GetDraftsForUser` (draft_store.go:125).
    fn get_drafts_for_user(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<Draft>, StoreError>> + Send;
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
