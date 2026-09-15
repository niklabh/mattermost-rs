//! Port of `SqlFileInfoStore` (channels/store/sqlstore/file_info_store.go): `Save`, `GetByIds`,
//! `Get`, `GetForPost`, `AttachToPost` and `DeleteForPost`.
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
use mm_model::file_info_list::FileInfoList;
use mm_model::search_params::{SearchParams, is_search_params_list_valid};
use sqlx::PgPool;

use crate::error::StoreError;
use crate::post_store::{
    SPECIAL_SEARCH_CHARS, mark_wildcards, remove_non_alpha_numeric_unquoted_terms,
};

/// Port of `store.FileInfoStore`, narrowed to what the served routes reach.
pub trait FileInfoStore {
    /// Port of `SqlFileInfoStore.Save` (file_info_store.go:113) — the row an upload creates.
    ///
    /// `PreSave`, `IsValid` (returned as [`StoreError::Invalid`], which the app layer passes
    /// through as the model's own 400 — Go's `errors.As(err, &appErr)`), then an `INSERT` of
    /// **twenty** columns: every one but `Archived`, which is left to its `false` default. So an
    /// `Archived` a caller set is discarded by the write and read back as `false`.
    ///
    /// `Width` and `Height` are `int` in Go and `integer` in Postgres; a value past `i32` is a
    /// driver error there and [`StoreError::Argument`] here, and no decoder this port trusts
    /// produces one.
    fn save(
        &self,
        info: FileInfo,
    ) -> impl std::future::Future<Output = Result<FileInfo, StoreError>> + Send;

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

    /// Port of `SqlFileInfoStore.GetForPost` (file_info_store.go:354) — the rows whose `PostId`
    /// **is** `post_id`, oldest first, which is not the same set as `get_by_ids(post.file_ids)`:
    /// a file re-parented by `attach_to_post` is in both, a file id a client listed but never
    /// attached is only in the second, and a file attached under an earlier version of the post
    /// is only in the first.
    ///
    /// `SendNotifications` reads this one for the `otherFile`/`image` keys on the `posted` event,
    /// with `includeDeleted` false — so a soft-deleted image that was nonetheless attached
    /// (`attach_to_post` does not test `DeleteAt`) is counted by `FileIds` but not by this read,
    /// and the event says `otherFile` without `image`.
    ///
    /// Go's `readFromMaster` and `allowFromCache` are dropped: one pool, no cache ([D-087]).
    fn get_for_post(
        &self,
        post_id: &str,
        include_deleted: bool,
    ) -> impl std::future::Future<Output = Result<Vec<FileInfo>, StoreError>> + Send;

    /// Port of `SqlFileInfoStore.AttachToPost` (file_info_store.go:405) — the write behind a
    /// `file_ids` list on `POST /posts`.
    ///
    /// One `UPDATE` with three predicates and a row count: the id, an **empty** `PostId` (a file
    /// attaches once; a second attempt, including the same id listed twice, matches no row), and a
    /// `CreatorId` that is either the poster or the literal `nouser` an upload with no user
    /// carries (app/file.go:628). It writes `ChannelId` as well as `PostId` — the upload already
    /// wrote the same channel, so for a REST upload the second column is a repeat. `DeleteAt` is
    /// not tested, so a soft-deleted file attaches.
    ///
    /// No row matched is [`StoreError::InvalidInput`], which `App.attachFileIDsToPost` logs and
    /// skips — the file is left out of the post rather than the post refused.
    fn attach_to_post(
        &self,
        file_id: &str,
        post_id: &str,
        channel_id: &str,
        creator_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlFileInfoStore.DeleteForPost` (file_info_store.go:457) — the soft delete
    /// `App.DeletePost` runs for a deleted post's own attachments.
    ///
    /// It stamps `DeleteAt` and **not** `UpdateAt`, so a withdrawn file still says when its
    /// metadata last changed, and it uses its own clock rather than the post's delete timestamp —
    /// the two differ by however long the delete took.
    ///
    /// Go returns the post id it was given and every caller ignores it.
    fn delete_for_post(
        &self,
        post_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlFileInfoStore.Search` (file_info_store.go:537) — the database file search
    /// behind `POST /api/v4/files/search` and `POST /api/v4/teams/{team_id}/files/search`.
    ///
    /// `page > 0` is an empty list before anything is read ("we don't support paging for DB
    /// search"); `IsSearchParamsListValid` is [`StoreError::Invalid`]. `per_page` is not a
    /// parameter because Go never reads it past that first `if`.
    fn search(
        &self,
        params_list: Vec<SearchParams>,
        user_id: &str,
        team_id: &str,
        page: i64,
    ) -> impl std::future::Future<Output = Result<FileInfoList, StoreError>> + Send;
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
    #[tracing::instrument(skip_all, fields(file_id, name = %info.name))]
    async fn save(&self, mut info: FileInfo) -> Result<FileInfo, StoreError> {
        info.pre_save();
        if let Err(app_error) = info.is_valid() {
            return Err(StoreError::Invalid {
                entity: "FileInfo",
                app_error,
            });
        }
        tracing::Span::current().record("file_id", &info.id);

        let out_of_range = |_| StoreError::Argument {
            entity: "FileInfo",
            detail: "width or height does not fit the integer column",
        };
        let width = i32::try_from(info.width).map_err(out_of_range)?;
        let height = i32::try_from(info.height).map_err(out_of_range)?;

        sqlx::query!(
            r#"
            INSERT INTO fileinfo
                (id, creatorid, postid, channelid, createat, updateat, deleteat, path,
                 thumbnailpath, previewpath, name, extension, size, mimetype, width, height,
                 haspreviewimage, minipreview, content, remoteid)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17,
                    $18, $19, $20)
            "#,
            info.id,
            info.creator_id,
            info.post_id,
            info.channel_id,
            info.create_at,
            info.update_at,
            info.delete_at,
            info.path,
            info.thumbnail_path,
            info.preview_path,
            info.name,
            info.extension,
            info.size,
            info.mime_type,
            width,
            height,
            info.has_preview_image,
            info.mini_preview.as_deref(),
            info.content,
            info.remote_id.as_deref(),
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to save FileInfo".to_owned(),
            source,
        })?;

        Ok(info)
    }

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

    #[tracing::instrument(skip(self), fields(post_id = %post_id))]
    async fn delete_for_post(&self, post_id: &str) -> Result<(), StoreError> {
        sqlx::query!(
            "UPDATE fileinfo SET deleteat = $1 WHERE postid = $2",
            mm_model::utils::get_millis(),
            post_id,
        )
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|source| StoreError::Db {
            context: format!("failed to update FileInfo with postId={post_id}"),
            source,
        })
    }

    #[tracing::instrument(skip(self), fields(post_id = %post_id, include_deleted))]
    async fn get_for_post(
        &self,
        post_id: &str,
        include_deleted: bool,
    ) -> Result<Vec<FileInfo>, StoreError> {
        // The same twenty-one columns as `get_by_ids`, scanned into `model.FileInfo` directly in
        // Go — so here, unlike there, `archived` survives.
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
             WHERE fileinfo.postid = $1
               AND ($2 OR fileinfo.deleteat = 0)
             ORDER BY fileinfo.createat
            "#,
            post_id,
            include_deleted
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find FileInfos with postId={post_id}"),
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

    #[tracing::instrument(skip(self), fields(file_id = %file_id, post_id = %post_id))]
    async fn attach_to_post(
        &self,
        file_id: &str,
        post_id: &str,
        channel_id: &str,
        creator_id: &str,
    ) -> Result<(), StoreError> {
        let result = sqlx::query!(
            r#"
            UPDATE fileinfo
               SET postid = $1, channelid = $2
             WHERE id = $3
               AND postid = ''
               AND (creatorid = $4 OR creatorid = 'nouser')
            "#,
            post_id,
            channel_id,
            file_id,
            creator_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update FileInfo with id={file_id} and postId={post_id}"),
            source,
        })?;

        if result.rows_affected() == 0 {
            // Could not attach the file to the post.
            return Err(StoreError::InvalidInput {
                entity: "FileInfo",
                field: "<id, postId, creatorId>",
                value: format!("<{file_id}, {post_id}, {creator_id}>"),
            });
        }
        Ok(())
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

    /// # One statement, every params element ANDed into it
    ///
    /// Go's loop appends each element's predicates to the **same** builder — unlike
    /// `SqlPostStore`, which runs one query per element and merges. `ParseSearchParams` gives
    /// every element the same filters (`search_params.go:355-380` builds each from one flag
    /// set), so the channel, user, extension and date predicates are bound once from the first
    /// element; only the term clause differs per element, and those are bound as a `text[]`
    /// with an `ALL`-shaped `NOT EXISTS`, which is the same conjunction. `IncludeDeletedChannels`
    /// is the same on every element by `IsSearchParamsListValid`.
    ///
    /// # The term clause is not the post one
    ///
    /// Three differences from `SqlPostStore.search`, each Go's own text (file_info_store.go:633-
    /// 670): the special characters are blanked in `Terms` whether or not the element is a
    /// hashtag search; every `-` becomes a space rather than only the non-word ones (the comment
    /// about `photo-2024.jpg` is Go's); and there is no quoted-phrase pass — `strings.Fields`
    /// splits on whitespace and joins with ` & ` / ` | `, and the excluded terms become
    /// ` & !(a | b)`. Whatever this builds is bound and parsed by Postgres, so a query Postgres
    /// rejects (a terms string that is all spaces builds `()`) is logged and answered as an
    /// **empty list**, exactly as Go swallows the store error.
    ///
    /// # `archived` is dropped, as in `get_by_ids`
    ///
    /// The rows scan into `fileInfoWithChannelID` and go through `ToModel()`, which omits
    /// `Archived` — see the module docs.
    #[tracing::instrument(skip(self, params_list), fields(team_id = %team_id, elements = params_list.len(), page))]
    async fn search(
        &self,
        mut params_list: Vec<SearchParams>,
        user_id: &str,
        team_id: &str,
        page: i64,
    ) -> Result<FileInfoList, StoreError> {
        if page > 0 {
            return Ok(FileInfoList::new());
        }

        is_search_params_list_valid(&params_list).map_err(|app_error| StoreError::Invalid {
            entity: "SearchParams",
            app_error,
        })?;

        let mut list = FileInfoList::new();
        let Some(first) = params_list.first() else {
            // Go's loop adds nothing and the base query runs; the app never sends an empty list
            // (it answers before the store), so this arm is a type-level courtesy.
            list.make_non_nil();
            return Ok(list);
        };

        // `buildCreateDateFilterClause`'s shape, inline: `on:` returns early, so none of the
        // other five bounds is applied beside it.
        let mut on_date = None;
        let mut excluded_date = None;
        let mut after_date = None;
        let mut before_date = None;
        let mut excluded_after_date = None;
        let mut excluded_before_date = None;
        if !first.on_date.is_empty() {
            on_date = Some(first.get_on_date_millis());
        } else {
            if !first.excluded_date.is_empty() {
                excluded_date = Some(first.get_excluded_date_millis());
            }
            if !first.after_date.is_empty() {
                after_date = Some(first.get_after_date_millis());
            }
            if !first.before_date.is_empty() {
                before_date = Some(first.get_before_date_millis());
            }
            if !first.excluded_after_date.is_empty() {
                excluded_after_date = Some(first.get_excluded_after_date_millis());
            }
            if !first.excluded_before_date.is_empty() {
                excluded_before_date = Some(first.get_excluded_before_date_millis());
            }
        }

        fn non_empty(list: &[String]) -> Option<&[String]> {
            (!list.is_empty()).then_some(list)
        }
        let include_deleted_channels = first.include_deleted_channels;
        let in_channels = non_empty(&first.in_channels).map(<[String]>::to_vec);
        let excluded_channels = non_empty(&first.excluded_channels).map(<[String]>::to_vec);
        let extensions = non_empty(&first.extensions).map(<[String]>::to_vec);
        let excluded_extensions = non_empty(&first.excluded_extensions).map(<[String]>::to_vec);
        let from_users = non_empty(&first.from_users).map(<[String]>::to_vec);
        let excluded_users = non_empty(&first.excluded_users).map(<[String]>::to_vec);

        let mut ts_queries: Vec<String> = Vec::with_capacity(params_list.len());
        for params in &mut params_list {
            params.terms = remove_non_alpha_numeric_unquoted_terms(&params.terms, " ");
            if let Some(query) =
                file_ts_query(&params.terms, &params.excluded_terms, params.or_terms)
            {
                ts_queries.push(query);
            }
        }
        let ts_queries = (!ts_queries.is_empty()).then_some(ts_queries);

        let text_config = crate::channel_store::default_text_search_config(&self.pool).await?;

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
                   COALESCE(fileinfo.remoteid, '')        AS "remote_id!"
              FROM fileinfo
              LEFT JOIN channels AS c ON c.id = fileinfo.channelid
              LEFT JOIN channelmembers AS cm ON c.id = cm.channelid
             WHERE fileinfo.deleteat = 0
               AND (fileinfo.creatorid = 'bookmark' OR fileinfo.postid <> '')
               AND NOT EXISTS (SELECT 1 FROM temporaryposts
                                WHERE temporaryposts.postid = fileinfo.postid)
               AND ($1::text = '' OR c.teamid = $1 OR c.teamid = '')
               AND ($2::bool OR c.deleteat = 0)
               AND cm.userid = $3
               AND ($4::text[] IS NULL OR c.id = ANY($4))
               AND ($5::text[] IS NULL OR fileinfo.extension = ANY($5))
               AND ($6::text[] IS NULL OR fileinfo.extension <> ALL($6))
               AND ($7::text[] IS NULL OR c.id <> ALL($7))
               AND ($8::text[] IS NULL OR fileinfo.creatorid = ANY($8))
               AND ($9::text[] IS NULL OR fileinfo.creatorid <> ALL($9))
               AND ($10::bigint IS NULL OR fileinfo.createat BETWEEN $10 AND $11::bigint)
               AND ($12::bigint IS NULL OR fileinfo.createat NOT BETWEEN $12 AND $13::bigint)
               AND ($14::bigint IS NULL OR fileinfo.createat >= $14)
               AND ($15::bigint IS NULL OR fileinfo.createat <= $15)
               AND ($16::bigint IS NULL OR fileinfo.createat < $16)
               AND ($17::bigint IS NULL OR fileinfo.createat > $17)
               AND ($18::text[] IS NULL
                    OR NOT EXISTS (
                        SELECT 1 FROM unnest($18::text[]) AS q
                         WHERE NOT (to_tsvector($19::text::regconfig, fileinfo.name)
                                        @@ to_tsquery($19::text::regconfig, q)
                                    OR to_tsvector($19::text::regconfig,
                                                   translate(fileinfo.name, '.,-', '   '))
                                        @@ to_tsquery($19::text::regconfig, q)
                                    OR to_tsvector($19::text::regconfig, fileinfo.content)
                                        @@ to_tsquery($19::text::regconfig, q))))
             ORDER BY fileinfo.createat DESC
             LIMIT 100
            "#,
            team_id,
            include_deleted_channels,
            user_id,
            in_channels.as_deref(),
            extensions.as_deref(),
            excluded_extensions.as_deref(),
            excluded_channels.as_deref(),
            from_users.as_deref(),
            excluded_users.as_deref(),
            on_date.map(|(start, _)| start),
            on_date.map(|(_, end)| end),
            excluded_date.map(|(start, _)| start),
            excluded_date.map(|(_, end)| end),
            after_date,
            before_date,
            excluded_after_date,
            excluded_before_date,
            ts_queries.as_deref(),
            text_config,
        )
        .fetch_all(&self.pool)
        .await;

        match rows {
            Err(source) => {
                // Go: `mlog.Warn("Query error searching files.", ...)` and an empty list —
                // "it is of no use to the user".
                tracing::warn!(error = %source, "query error searching files");
            }
            Ok(rows) => {
                for row in rows {
                    let info = FileInfo {
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
                        // `ToModel()` omits this field — see the module docs.
                        archived: false,
                    };
                    list.add_order(info.id.as_str());
                    list.add_file_info(info);
                }
            }
        }
        list.make_non_nil();
        Ok(list)
    }
}

/// The tsquery text one params element contributes (file_info_store.go:633-670), or `None` when
/// both term strings are empty after the character blanking — "we've already confirmed that we
/// have a channel or user to search for".
///
/// Go tests emptiness **after** the special characters and hyphens are blanked but **before**
/// the wildcard pass, so a terms string of `-` is ` `, which is not empty, and builds `()`.
pub(crate) fn file_ts_query(terms: &str, excluded_terms: &str, or_terms: bool) -> Option<String> {
    let mut terms = terms.to_owned();
    let mut excluded_terms = excluded_terms.to_owned();
    for c in SPECIAL_SEARCH_CHARS {
        terms = terms.replace(c, " ");
        excluded_terms = excluded_terms.replace(c, " ");
    }
    terms = terms.replace('-', " ");
    excluded_terms = excluded_terms.replace('-', " ");

    if terms.is_empty() && excluded_terms.is_empty() {
        return None;
    }

    let terms = mark_wildcards(&terms);
    let excluded_terms = mark_wildcards(&excluded_terms);

    let mut exclude_clause = String::new();
    if !excluded_terms.is_empty() {
        exclude_clause = format!(
            " & !({})",
            excluded_terms
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }
    let joiner = if or_terms { " | " } else { " & " };
    Some(format!(
        "({}){exclude_clause}",
        terms.split_whitespace().collect::<Vec<_>>().join(joiner)
    ))
}

#[cfg(test)]
mod tests {
    use super::file_ts_query;

    /// Transcribed from file_info_store.go:633-670: the helpers are unexported and the parity
    /// suite is their oracle; these pin the string the suite then sends through Postgres.
    #[test]
    fn the_file_ts_query_is_gos_text() {
        assert_eq!(
            file_ts_query("photo 2024", "", false).as_deref(),
            Some("(photo & 2024)")
        );
        assert_eq!(
            file_ts_query("photo 2024", "", true).as_deref(),
            Some("(photo | 2024)")
        );
        // Hyphens become spaces whatever flanks them, unlike the post search.
        assert_eq!(
            file_ts_query("photo-2024.jpg", "", false).as_deref(),
            Some("(photo & 2024.jpg)")
        );
        // A star at a word end is a prefix; excluded terms are always OR-joined and negated.
        assert_eq!(
            file_ts_query("pho* report", "old draft", false).as_deref(),
            Some("(pho:* & report) & !(old | draft)")
        );
        // Special characters are blanked before the split.
        assert_eq!(
            file_ts_query("a:b(c)", "", false).as_deref(),
            Some("(a & b & c)")
        );
        // Emptiness is tested after the blanking: `-` alone builds `()`, which Postgres rejects.
        assert_eq!(file_ts_query("-", "", false).as_deref(), Some("()"));
        assert_eq!(file_ts_query("", " ", false).as_deref(), Some("() & !()"));
        assert_eq!(file_ts_query("", "", false), None);
    }
}
