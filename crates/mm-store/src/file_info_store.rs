//! Port of `SqlFileInfoStore` (channels/store/sqlstore/file_info_store.go), `GetByIds` only.
//!
//! Unblocks `metadata.files` on `GET /api/v4/posts/{post_id}`.

use mm_model::file_info::FileInfo;
use sqlx::PgPool;

use crate::error::StoreError;

/// Port of `store.FileInfoStore`, narrowed to the one read a post handler makes.
pub trait FileInfoStore {
    /// Port of `SqlFileInfoStore.GetByIds` (file_info_store.go:135).
    fn get_by_ids(
        &self,
        ids: &[String],
        include_deleted: bool,
    ) -> impl std::future::Future<Output = Result<Vec<FileInfo>, StoreError>> + Send;
}

#[derive(Debug, Clone)]
pub struct SqlFileInfoStore {
    pool: PgPool,
}

impl SqlFileInfoStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl FileInfoStore for SqlFileInfoStore {
    /// # `ORDER BY CreateAt DESC` is not the order a client sees
    ///
    /// Go sorts newest-first here and then **re-orders the result by `post.FileIds`** in
    /// `orderFileInfosByID` (app/post.go:2433). The SQL order only decides the tail: ids that
    /// are in the result but not in `FileIds` keep it. Both halves have to be right, and
    /// dropping this `ORDER BY` is invisible for any post whose files are all listed in
    /// `FileIds` — which is every post the API can create.
    ///
    /// # `post.FileIds` is stored **sorted**
    ///
    /// `Post.PreSave` ends with `o.FileIds = RemoveDuplicateStrings(o.FileIds)` (post.go:740),
    /// and that helper sorts before deduplicating — so the order a client sends is discarded and
    /// what reaches this query, and then `orderFileInfosByID`, is alphabetical by id. Ids are
    /// random, so whether that coincides with `CreateAt DESC` is luck; do not read a passing
    /// ordering test as proof the reorder ran. The parity fixture plants the column to force the
    /// two orders apart, because on the first run they happened to agree.
    ///
    /// # Three columns are coalesced and three are not
    ///
    /// `ChannelId`, `Content` and `RemoteId` fall back to `''`; `Width`, `Height` and
    /// `MiniPreview` do not, because Go models them as nullable-tolerant types. Note that
    /// `RemoteId` is `COALESCE`d to the empty string and then held in a `*string`, so it is
    /// **never nil** out of this query — `"remote_id":""`, not an omitted key.
    #[tracing::instrument(skip(self), fields(count = ids.len(), include_deleted))]
    async fn get_by_ids(
        &self,
        ids: &[String],
        include_deleted: bool,
    ) -> Result<Vec<FileInfo>, StoreError> {
        // Go appends `AND FileInfo.DeleteAt = 0` only when `!includeDeleted`; expressed as a
        // parameter for the same reason as `SqlPostStore::get_single`'s.
        let rows = sqlx::query!(
            r#"
            SELECT fileinfo.id                            AS "id!",
                   fileinfo.creatorid                     AS "creator_id!",
                   fileinfo.postid                        AS "post_id!",
                   COALESCE(fileinfo.channelid, '')       AS "channel_id!",
                   fileinfo.createat                      AS "create_at!",
                   fileinfo.updateat                      AS "update_at!",
                   fileinfo.deleteat                      AS "delete_at!",
                   fileinfo.path                          AS "path!",
                   fileinfo.thumbnailpath                 AS "thumbnail_path!",
                   fileinfo.previewpath                   AS "preview_path!",
                   fileinfo.name                          AS "name!",
                   fileinfo.extension                     AS "extension!",
                   fileinfo.size                          AS "size!",
                   fileinfo.mimetype                      AS "mime_type!",
                   fileinfo.width                         AS "width!",
                   fileinfo.height                        AS "height!",
                   fileinfo.haspreviewimage               AS "has_preview_image!",
                   fileinfo.minipreview                   AS "mini_preview?",
                   COALESCE(fileinfo.content, '')         AS "content!",
                   COALESCE(fileinfo.remoteid, '')        AS "remote_id!",
                   fileinfo.archived                      AS "archived!"
              FROM fileinfo
             WHERE fileinfo.id = ANY($1)
               AND ($2 OR fileinfo.deleteat = 0)
             ORDER BY fileinfo.createat DESC
            "#,
            ids,
            include_deleted
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find FileInfos".to_owned(),
            source,
        })?;

        Ok(rows
            .into_iter()
            .map(|row| FileInfo {
                id: row.id,
                creator_id: row.creator_id,
                post_id: row.post_id,
                channel_id: row.channel_id,
                create_at: row.create_at,
                update_at: row.update_at,
                delete_at: row.delete_at,
                path: row.path,
                thumbnail_path: row.thumbnail_path,
                preview_path: row.preview_path,
                name: row.name,
                extension: row.extension,
                size: row.size,
                mime_type: row.mime_type,
                width: i64::from(row.width),
                height: i64::from(row.height),
                has_preview_image: row.has_preview_image,
                mini_preview: row.mini_preview,
                content: row.content,
                remote_id: Some(row.remote_id),
                archived: row.archived,
            })
            .collect())
    }
}
