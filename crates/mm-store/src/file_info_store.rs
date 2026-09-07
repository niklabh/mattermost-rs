//! Port of `SqlFileInfoStore` (channels/store/sqlstore/file_info_store.go): `GetByIds` and
//! `Get`.
//!
//! `GetByIds` unblocks `metadata.files` on `GET /api/v4/posts/{post_id}` and the whole of
//! `GET /api/v4/posts/{post_id}/files/info`; `Get` unblocks `GET /api/v4/files/{file_id}/info`.
//!
//! # The two reads disagree about `archived`, and Go is the one being inconsistent
//!
//! Both queries select the same twenty-one columns from `fs.queryFields` (file_info_store.go:86),
//! `FileInfo.Archived` among them. They then scan into **different Go types**:
//!
//! - `Get` scans straight into `model.FileInfo`, so the column reaches the wire.
//! - `GetByIds` scans into the store-private `fileInfoWithChannelID` and converts with
//!   `ToModel()` (file_info_store.go:75) — which assigns twenty of the twenty-one fields and
//!   **silently omits `Archived`**. Every `FileInfo` that leaves `GetByIds` therefore reports
//!   `"archived":false`, whatever the row says.
//!
//! `archived` is `json:"archived"` with no `omitempty`, so both answers are on the wire and they
//! differ. This port reproduces the omission rather than the intent: see
//! [`SqlFileInfoStore::get_by_ids`].
//!
//! In practice the column is `false` for every row the API can create — `Save` (file_info_store.go:113)
//! does not list `Archived` among its INSERT columns, and the only writer of `true` is
//! `FileInfo.MakeContentInaccessible` (model/file_info.go:246), which mutates an already-loaded
//! struct for the cloud file limit and never reaches Postgres. So the divergence is invisible
//! until someone writes the column directly, which is exactly what the parity fixture does.

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

    /// Port of `SqlFileInfoStore.Get` (file_info_store.go:248), which is `get(id, false)`.
    ///
    /// Go's `fromMaster` parameter is dropped: it selects the writer connection, and this port
    /// has one pool.
    fn get(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<FileInfo, StoreError>> + Send;

    /// Port of `SqlFileInfoStore.GetStorageUsage` (file_info_store.go:739).
    ///
    /// Go's signature is `GetStorageUsage(_, includeDeleted bool)` — the **first** parameter is
    /// unnamed and unused, so `GetStorageUsage(true, false)` in `App.GetStorageUsage` reads as
    /// though it asked for something it did not. Only the second argument does anything, and it
    /// is the one this takes.
    fn get_storage_usage(
        &self,
        include_deleted: bool,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;
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
    /// # Two different sources, and the default one is a materialized view
    ///
    /// With `includeDeleted` false — the only value `App.GetStorageUsage` passes — Go reads
    /// `SELECT usage FROM file_stats`, a **materialized view** the Go server refreshes on a
    /// schedule. So the answer is as stale as the last refresh, on both servers equally, and
    /// summing `FileInfo.Size` here instead would make us *more* current than Go and produce a
    /// parity failure that looks like a bug in the sum.
    ///
    /// With it true the query is `COALESCE(SUM(Size), 0) FROM FileInfo` — live, and over deleted
    /// rows as well. Nothing migrated passes true; it is here because the two halves are one
    /// function in Go and splitting them invites the next caller to guess which it got.
    ///
    /// `file_stats` has exactly one row on a migrated schema. `fetch_one` therefore matches Go's
    /// `Get`, which errors on no rows — an empty view is a broken migration, not an empty server.
    ///
    /// # The cast to `bigint` is Go's scan, written down
    ///
    /// `SUM(Size)` is `numeric` in Postgres and so is `file_stats.usage`, while Go scans both
    /// into a plain `int64` and lets `lib/pq` narrow them. sqlx refuses to guess and asks for
    /// `bigdecimal`; casting in the query is the same narrowing Go's driver performs, at the same
    /// point, and keeps the compile-time checker. Both servers therefore truncate identically —
    /// and neither can represent a total above `i64::MAX`, which is nine exabytes of files.
    #[tracing::instrument(skip_all, fields(include_deleted, bytes))]
    async fn get_storage_usage(&self, include_deleted: bool) -> Result<i64, StoreError> {
        let bytes = if include_deleted {
            sqlx::query_scalar!(
                r#"SELECT COALESCE(SUM(size), 0)::bigint AS "usage!" FROM fileinfo"#
            )
            .fetch_one(&self.pool)
            .await
        } else {
            sqlx::query_scalar!(r#"SELECT COALESCE(usage, 0)::bigint AS "usage!" FROM file_stats"#)
                .fetch_one(&self.pool)
                .await
        }
        .map_err(|source| StoreError::Db {
            context: "failed to get storage usage".to_owned(),
            source,
        })?;

        tracing::Span::current().record("bytes", bytes);
        Ok(bytes)
    }

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
    ///
    /// # `archived` is discarded, on purpose
    ///
    /// The column is selected — Go selects it too, in `fs.queryFields` — and then thrown away,
    /// because Go's `fileInfoWithChannelID.ToModel()` (file_info_store.go:75) never assigns it.
    /// So this read always answers `"archived":false` while [`SqlFileInfoStore::get`], one
    /// function below, answers the row's real value from the *same* column. Selecting it and
    /// dropping it, rather than leaving it out of the query, is what keeps the two halves of
    /// that bug visible in one place; see the module docs.
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
                // `ToModel()` omits this field. Not `row.archived` — see the doc comment.
                archived: false,
            })
            .collect())
    }

    /// # `Get` is not `GetByIds` with one id
    ///
    /// Three differences, and each of them is observable:
    ///
    /// 1. **`DeleteAt = 0` is unconditional.** There is no `includeDeleted` parameter, so a
    ///    soft-deleted file is a 404 here however the caller asks.
    /// 2. **A miss is `ErrNotFound`**, not an empty list — `getFileInfo` turns it into a 404,
    ///    where `getFileInfosForPost` answers `[]` for a post with no files.
    /// 3. **`archived` survives.** Go scans this query into `model.FileInfo` directly, so the
    ///    column reaches the wire; the sibling read drops it. See the module docs.
    #[tracing::instrument(skip(self), fields(file_id = %id))]
    async fn get(&self, id: &str) -> Result<FileInfo, StoreError> {
        let row = sqlx::query!(
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
             WHERE fileinfo.id = $1
               AND fileinfo.deleteat = 0
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get FileInfo with id={id}"),
            source,
        })?
        .ok_or_else(|| StoreError::NotFound {
            entity: "FileInfo",
            criteria: id.to_owned(),
        })?;

        Ok(FileInfo {
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
    }
}
