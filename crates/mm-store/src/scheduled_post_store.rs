//! Port of `SqlScheduledPostStore` (channels/store/sqlstore/scheduled_post_store.go) — the six
//! methods the four api4 routes (`api4/scheduled_post.go`) and `App.PermanentDeleteUser` reach.
//!
//! The job's three (`GetPendingScheduledPosts`, `UpdateOldScheduledPosts`,
//! `UpdateRecurringScheduledPosts`) are not ported: no route calls them.
//!
//! # Four columns are JSON text, and `Type` is the one Go coalesces
//!
//! `Props`, `FileIds` and `Priority` are `varchar`/`text` holding JSON, written with Go's
//! `StringInterfaceToJSON`/`ArrayToJSON` (HTML-escaped, `null` for nil) and read back with the
//! draft store's scanners — the same `StringInterface.Scan` that turns a NULL `Props` into `{}`.
//! `Type` is nullable, left out of `baseColumns` and read as `COALESCE(Type, '')`.
//!
//! # `DeleteAt` is not a column
//!
//! The embedded `Draft` has one; `ScheduledPosts` does not. Every read leaves it at zero and
//! every write drops it, so `"delete_at": 0` on the wire whatever a client sent.

use mm_model::scheduled_post::ScheduledPost;
use mm_model::utils::{array_to_json, get_millis, string_interface_to_json};
use sqlx::PgPool;

use crate::draft_store::{decode_array, decode_map};
use crate::error::StoreError;

/// `model.PostMessageMaxRunesV2` — what `newScheduledPostStore` seeds `maxMessageSizeCached`
/// with. Never observable from the wire: see [`ScheduledPostStore::get_max_message_size`].
pub const POST_MESSAGE_MAX_RUNES_V2: i64 = 16383;

/// The subset of Go's `store.ScheduledPostStore` (store/store.go) that is ported.
pub trait ScheduledPostStore {
    /// Port of `SqlScheduledPostStore.PermanentDeleteByUser` (scheduled_post_store.go:355).
    ///
    /// Every scheduled post the user owns, sent, failed or pending alike. One `DELETE`, no
    /// `RowsAffected` check. Go also logs the failure at error level before returning it.
    fn permanent_delete_by_user(
        &self,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlScheduledPostStore.CreateScheduledPost` (scheduled_post_store.go:89).
    ///
    /// **Runs `PreSave` again**, on the caller's value: `App.SaveScheduledPost` already has, so
    /// the id survives (it is only minted when empty) but `update_at` is re-stamped, and the
    /// value the handler answers with is the one mutated here. A replacement from
    /// `ScheduledPostWillBeCreated` that carried no id is given one at this point.
    fn create_scheduled_post(
        &self,
        scheduled_post: &mut ScheduledPost,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlScheduledPostStore.GetScheduledPostsForUser` (scheduled_post_store.go:111).
    ///
    /// An inner join on `Channels`, **not** on `ChannelMembers`: Go's comment says a post in a
    /// channel the user has left, or one since archived, is still listed so the client can show
    /// why it will not send. The empty `team_id` is how the direct and group channels are asked
    /// for. `ORDER BY ScheduledAt, CreateAt` — ascending, with no id tiebreak.
    fn get_scheduled_posts_for_user(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<ScheduledPost>, StoreError>> + Send;

    /// Port of `SqlScheduledPostStore.Get` (scheduled_post_store.go:317).
    ///
    /// **A missing row is an error, not `(nil, nil)`.** `GetBuilder` returns `sql.ErrNoRows`, and
    /// Go wraps it like any driver failure — so the `existingScheduledPost == nil` branches in
    /// the api4 handlers and the app layer are unreachable, and an unknown id is their **500**.
    /// [`StoreError::NotFound`] carries that case so a caller can log it apart, but every caller
    /// ported answers it exactly as it answers a driver error.
    fn get(
        &self,
        scheduled_post_id: &str,
    ) -> impl std::future::Future<Output = Result<ScheduledPost, StoreError>> + Send;

    /// Port of `SqlScheduledPostStore.UpdatedScheduledPost` (scheduled_post_store.go:249).
    ///
    /// Calls `PreUpdate` on the caller's value — `update_at` becomes the clock — and then writes
    /// `UpdateAt` from **a second `GetMillis()`** (`toUpdateMap`), so the row and the value the
    /// handler answers with can differ by a millisecond. Eleven columns; `CreateAt`, `UserId`,
    /// `ChannelId` and `RootId` are never written. No `RowsAffected` check: an id naming no row
    /// updates nothing and succeeds.
    fn updated_scheduled_post(
        &self,
        scheduled_post: &mut ScheduledPost,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlScheduledPostStore.PermanentlyDeleteScheduledPosts`
    /// (scheduled_post_store.go:222). An empty list returns before any query.
    fn permanently_delete_scheduled_posts(
        &self,
        scheduled_post_ids: &[String],
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlScheduledPostStore.GetMaxMessageSize` (scheduled_post_store.go:141).
    ///
    /// `determineMaxColumnSize("ScheduledPosts", "Message")`: the column's
    /// `character_maximum_length` over four, 65535/4 = 16383 on the stack. Memoised once per
    /// store like Go's `sync.Once`. **A failure memoises zero**, not the seeded
    /// [`POST_MESSAGE_MAX_RUNES_V2`]: Go assigns both results of the call
    /// (`s.maxMessageSizeCached, err = …`), and the error path returns `0`, so every non-empty
    /// message is then refused.
    fn get_max_message_size(&self) -> impl std::future::Future<Output = i64> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlScheduledPostStore {
    pool: PgPool,
    /// Go's `maxMessageSizeOnce` and `maxMessageSizeCached` together. Shared by every clone, as
    /// the one Go store is shared by every caller.
    max_message_size: std::sync::Arc<tokio::sync::OnceCell<i64>>,
}

impl SqlScheduledPostStore {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            max_message_size: std::sync::Arc::new(tokio::sync::OnceCell::new()),
        }
    }
}

/// One row as `columnsForRead` selects it, before the three JSON columns are parsed.
struct Row {
    id: String,
    create_at: i64,
    update_at: i64,
    user_id: String,
    channel_id: String,
    root_id: String,
    message: String,
    props: Option<String>,
    file_ids: Option<String>,
    priority: Option<String>,
    scheduled_at: i64,
    processed_at: i64,
    error_code: String,
    repeat_type: String,
    repeat_timezone: String,
    draft_type: String,
}

impl Row {
    fn into_model(self) -> Result<ScheduledPost, StoreError> {
        let mut post = ScheduledPost {
            id: self.id,
            scheduled_at: self.scheduled_at,
            processed_at: self.processed_at,
            error_code: self.error_code,
            repeat_type: self.repeat_type,
            repeat_timezone: self.repeat_timezone,
            ..ScheduledPost::default()
        };
        post.draft.create_at = self.create_at;
        post.draft.update_at = self.update_at;
        post.draft.user_id = self.user_id;
        post.draft.channel_id = self.channel_id;
        post.draft.root_id = self.root_id;
        post.draft.message = self.message;
        post.draft.draft_type = self.draft_type;
        post.draft.props = decode_map("props", self.props)?;
        post.draft.file_ids = decode_array("fileids", self.file_ids)?;
        post.draft.priority = decode_map("priority", self.priority)?;
        Ok(post)
    }
}

fn db_error(context: &str, source: sqlx::Error) -> StoreError {
    tracing::error!(error = %source, "{context}");
    StoreError::Db {
        context: context.to_owned(),
        source,
    }
}

impl ScheduledPostStore for SqlScheduledPostStore {
    #[tracing::instrument(skip_all, fields(user_id = %user_id, deleted))]
    async fn permanent_delete_by_user(&self, user_id: &str) -> Result<(), StoreError> {
        let result = sqlx::query!("DELETE FROM scheduledposts WHERE userid = $1", user_id)
            .execute(&self.pool)
            .await
            .map_err(|source| {
                tracing::error!(error = %source, "PermanentDeleteByUser: failed to delete scheduled posts by user from database");
                StoreError::Db {
                    context: "PermanentDeleteByUser: failed to delete scheduled posts by user from database".to_owned(),
                    source,
                }
            })?;
        tracing::Span::current().record("deleted", result.rows_affected());
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(user_id = %scheduled_post.user_id))]
    async fn create_scheduled_post(
        &self,
        scheduled_post: &mut ScheduledPost,
    ) -> Result<(), StoreError> {
        scheduled_post.pre_save();
        let props = string_interface_to_json(scheduled_post.get_props());
        let file_ids = array_to_json(scheduled_post.file_ids.as_deref());
        let priority = string_interface_to_json(scheduled_post.priority.as_ref());
        sqlx::query!(
            r#"
            INSERT INTO scheduledposts
                (id, createat, updateat, userid, channelid, rootid, message, props, fileids,
                 priority, scheduledat, processedat, errorcode, repeattype, repeattimezone, type)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
            "#,
            scheduled_post.id,
            scheduled_post.create_at,
            scheduled_post.update_at,
            scheduled_post.user_id,
            scheduled_post.channel_id,
            scheduled_post.root_id,
            scheduled_post.message,
            props,
            file_ids,
            priority,
            scheduled_post.scheduled_at,
            scheduled_post.processed_at,
            scheduled_post.error_code,
            scheduled_post.repeat_type,
            scheduled_post.repeat_timezone,
            scheduled_post.draft_type,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| {
            db_error(
                "SqlScheduledPostStore.CreateScheduledPost failed to insert scheduled post",
                source,
            )
        })?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(found))]
    async fn get_scheduled_posts_for_user(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> Result<Vec<ScheduledPost>, StoreError> {
        let rows = sqlx::query_as!(
            Row,
            r#"
            SELECT sp.id                   AS "id!",
                   sp.createat             AS "create_at!",
                   sp.updateat             AS "update_at!",
                   sp.userid               AS "user_id!",
                   sp.channelid            AS "channel_id!",
                   sp.rootid               AS "root_id!",
                   sp.message              AS "message!",
                   sp.props                AS "props?",
                   sp.fileids              AS "file_ids?",
                   sp.priority             AS "priority?",
                   sp.scheduledat          AS "scheduled_at!",
                   sp.processedat          AS "processed_at!",
                   sp.errorcode            AS "error_code!",
                   sp.repeattype           AS "repeat_type!",
                   sp.repeattimezone       AS "repeat_timezone!",
                   COALESCE(sp.type, '')   AS "draft_type!"
              FROM scheduledposts AS sp
              INNER JOIN channels AS c ON sp.channelid = c.id
             WHERE sp.userid = $1 AND c.teamid = $2
             ORDER BY sp.scheduledat, sp.createat
            "#,
            user_id,
            team_id,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| {
            db_error(
                "SqlScheduledPostStore.GetScheduledPostsForUser: failed to fetch scheduled posts for user",
                source,
            )
        })?;
        tracing::Span::current().record("found", rows.len());
        rows.into_iter().map(Row::into_model).collect()
    }

    #[tracing::instrument(skip(self))]
    async fn get(&self, scheduled_post_id: &str) -> Result<ScheduledPost, StoreError> {
        let row = sqlx::query_as!(
            Row,
            r#"
            SELECT id                   AS "id!",
                   createat             AS "create_at!",
                   updateat             AS "update_at!",
                   userid               AS "user_id!",
                   channelid            AS "channel_id!",
                   rootid               AS "root_id!",
                   message              AS "message!",
                   props                AS "props?",
                   fileids              AS "file_ids?",
                   priority             AS "priority?",
                   scheduledat          AS "scheduled_at!",
                   processedat          AS "processed_at!",
                   errorcode            AS "error_code!",
                   repeattype           AS "repeat_type!",
                   repeattimezone       AS "repeat_timezone!",
                   COALESCE(type, '')   AS "draft_type!"
              FROM scheduledposts
             WHERE id = $1
            "#,
            scheduled_post_id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| {
            db_error(
                "SqlScheduledPostStore.Get: failed to get single scheduled post by ID from database",
                source,
            )
        })?;
        match row {
            Some(row) => row.into_model(),
            None => Err(StoreError::NotFound {
                entity: "ScheduledPost",
                criteria: format!("scheduledPostId={scheduled_post_id}"),
            }),
        }
    }

    #[tracing::instrument(skip_all, fields(scheduled_post_id = %scheduled_post.id))]
    async fn updated_scheduled_post(
        &self,
        scheduled_post: &mut ScheduledPost,
    ) -> Result<(), StoreError> {
        scheduled_post.pre_update();
        // `toUpdateMap`'s own `model.GetMillis()`, not the one `PreUpdate` just wrote.
        let update_at = get_millis();
        let props = string_interface_to_json(scheduled_post.get_props());
        let file_ids = array_to_json(scheduled_post.file_ids.as_deref());
        let priority = string_interface_to_json(scheduled_post.priority.as_ref());
        sqlx::query!(
            r#"
            UPDATE scheduledposts
               SET updateat = $2, message = $3, props = $4, fileids = $5, priority = $6,
                   scheduledat = $7, processedat = $8, errorcode = $9, type = $10,
                   repeattype = $11, repeattimezone = $12
             WHERE id = $1
            "#,
            scheduled_post.id,
            update_at,
            scheduled_post.message,
            props,
            file_ids,
            priority,
            scheduled_post.scheduled_at,
            scheduled_post.processed_at,
            scheduled_post.error_code,
            scheduled_post.draft_type,
            scheduled_post.repeat_type,
            scheduled_post.repeat_timezone,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| {
            db_error(
                "SqlScheduledPostStore.UpdatedScheduledPost failed to update scheduled post",
                source,
            )
        })?;
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(count = scheduled_post_ids.len()))]
    async fn permanently_delete_scheduled_posts(
        &self,
        scheduled_post_ids: &[String],
    ) -> Result<(), StoreError> {
        if scheduled_post_ids.is_empty() {
            return Ok(());
        }
        sqlx::query!(
            "DELETE FROM scheduledposts WHERE id = ANY($1)",
            scheduled_post_ids,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| {
            db_error(
                "PermanentlyDeleteScheduledPosts: failed to delete batch of scheduled posts from database",
                source,
            )
        })?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn get_max_message_size(&self) -> i64 {
        *self
            .max_message_size
            .get_or_init(|| async {
                let bytes = sqlx::query_scalar!(
                    r#"
                    SELECT COALESCE(character_maximum_length, 0)::integer AS "length!"
                      FROM information_schema.columns
                     WHERE lower(table_name) = lower($1)
                       AND lower(column_name) = lower($2)
                    "#,
                    "ScheduledPosts",
                    "Message",
                )
                .fetch_one(&self.pool)
                .await;
                match bytes {
                    // "Assume a worst-case representation of four bytes per rune."
                    Ok(bytes) => i64::from(bytes) / 4,
                    Err(err) => {
                        tracing::error!(error = %err, "SqlScheduledPostStore.getMaxMessageSize: error occurred during determining max column size for ScheduledPosts.Message column");
                        0
                    }
                }
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pool that can never connect, with the acquire timeout capped so a test that reaches it
    /// fails in a second rather than sqlx's thirty.
    fn unreachable_store() -> SqlScheduledPostStore {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(200))
            .connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
            .unwrap();
        SqlScheduledPostStore::new(pool)
    }

    /// Go's `s.maxMessageSizeCached, err = …` overwrites the seeded default with the zero the
    /// failed lookup returned — so a store that cannot read the schema refuses every message,
    /// and keeps doing so, since the `sync.Once` has fired.
    #[tokio::test]
    async fn a_failed_size_lookup_memoises_zero_not_the_default() {
        let store = unreachable_store();
        assert_eq!(store.get_max_message_size().await, 0);
        assert_eq!(
            store.get_max_message_size().await,
            0,
            "memoised, not retried"
        );
        assert_ne!(POST_MESSAGE_MAX_RUNES_V2, 0);
    }

    /// An empty id list is not a query: it succeeds against a database that is not there.
    #[tokio::test]
    async fn deleting_nothing_touches_nothing() {
        let store = unreachable_store();
        assert!(store.permanently_delete_scheduled_posts(&[]).await.is_ok());
        assert!(
            store
                .permanently_delete_scheduled_posts(&["x".to_owned()])
                .await
                .is_err()
        );
    }

    /// NULL `Props` is `{}` and NULL `FileIds` is nil — the two scanners are not mirrors — and
    /// the text `null` in `Props` is nil.
    #[test]
    fn a_row_decodes_its_json_columns_as_go_scans_them() {
        let row = |props: Option<&str>, file_ids: Option<&str>| Row {
            id: "i".repeat(26),
            create_at: 1,
            update_at: 2,
            user_id: "u".repeat(26),
            channel_id: "c".repeat(26),
            root_id: String::new(),
            message: "m".to_owned(),
            props: props.map(str::to_owned),
            file_ids: file_ids.map(str::to_owned),
            priority: None,
            scheduled_at: 3,
            processed_at: 0,
            error_code: String::new(),
            repeat_type: String::new(),
            repeat_timezone: String::new(),
            draft_type: String::new(),
        };
        let post = row(None, None).into_model().unwrap();
        assert_eq!(post.props, Some(Default::default()));
        assert_eq!(post.file_ids, None);
        assert_eq!(post.delete_at, 0, "not a column");

        let post = row(Some("null"), Some(r#"["a","b"]"#))
            .into_model()
            .unwrap();
        assert_eq!(post.props, None);
        assert_eq!(post.file_ids, Some(vec!["a".to_owned(), "b".to_owned()]));

        assert!(row(Some("[]"), None).into_model().is_err());
    }
}
