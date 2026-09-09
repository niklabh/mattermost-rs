//! Port of `SqlUploadSessionStore` (channels/store/sqlstore/upload_session_store.go) — the two
//! reads.
//!
//! Ported for `getUpload` (`GET /api/v4/uploads/{upload_id}`) and `getUploadsForUser`
//! (`GET /api/v4/users/{user_id}/uploads`). The write half belongs to the upload flow itself,
//! which is three `POST`s and not migrated.
//!
//! # `Type` is a Postgres enum, not a varchar
//!
//! The column is `upload_session_type`, so it needs an explicit `::text` for sqlx to bind it to a
//! `String`. Go gets away without one because `sqlx`'s Go namesake hands the driver a `string`
//! and Postgres coerces on input; on the way *out* the wire type is the enum's oid, which the
//! Rust driver refuses to decode as text without the cast.
//!
//! # `Get` validates the id *before* querying, and the error is not a not-found
//!
//! `SqlUploadSessionStore.Get` returns a bare `errors.New("id is not valid")` for a malformed id
//! — not `ErrNotFound` — so `App.GetUploadSession` maps it to a **500**, while a well-formed id
//! that matches nothing is a 404. The two are one error id (`app.upload.get.app_error`) at two
//! statuses, and the handler's own `RequireUploadId` catches the malformed case first anyway.

use mm_model::upload_session::{UploadSession, UploadType};
use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `store.UploadSessionStore` the two read routes need.
pub trait UploadSessionStore {
    /// Port of `SqlUploadSessionStore.Get` (upload_session_store.go:102). `ErrNotFound` on a miss.
    fn get(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<UploadSession, StoreError>> + Send;

    /// Port of `SqlUploadSessionStore.GetForUser` (upload_session_store.go:122).
    ///
    /// `ORDER BY CreateAt ASC` — oldest first, which is the order the admin console shows and is
    /// not the default any other listing here uses. Go initialises with `[]*model.UploadSession{}`,
    /// the **empty-slice** form, so a user with no uploads marshals as `[]` and never `null`.
    fn get_for_user(
        &self,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<UploadSession>, StoreError>> + Send;
}

#[derive(Debug, Clone)]
pub struct SqlUploadSessionStore {
    pool: PgPool,
}

impl SqlUploadSessionStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

struct UploadSessionRow {
    id: String,
    type_: String,
    createat: i64,
    userid: String,
    channelid: String,
    filename: String,
    path: String,
    filesize: i64,
    fileoffset: i64,
    remoteid: String,
    reqfileid: String,
}

impl From<UploadSessionRow> for UploadSession {
    fn from(row: UploadSessionRow) -> Self {
        UploadSession {
            id: row.id,
            type_: UploadType(row.type_),
            create_at: row.createat,
            user_id: row.userid,
            channel_id: row.channelid,
            filename: row.filename,
            path: row.path,
            file_size: row.filesize,
            file_offset: row.fileoffset,
            remote_id: row.remoteid,
            req_file_id: row.reqfileid,
        }
    }
}

impl UploadSessionStore for SqlUploadSessionStore {
    #[tracing::instrument(skip_all, fields(upload_id = %id))]
    async fn get(&self, id: &str) -> Result<UploadSession, StoreError> {
        let row = sqlx::query_as!(
            UploadSessionRow,
            r#"
            SELECT id                            AS "id!",
                   COALESCE(type::text, '')      AS "type_!",
                   COALESCE(createat, 0)         AS "createat!",
                   COALESCE(userid, '')          AS "userid!",
                   COALESCE(channelid, '')       AS "channelid!",
                   COALESCE(filename, '')        AS "filename!",
                   COALESCE(path, '')            AS "path!",
                   COALESCE(filesize, 0)         AS "filesize!",
                   COALESCE(fileoffset, 0)       AS "fileoffset!",
                   COALESCE(remoteid, '')        AS "remoteid!",
                   COALESCE(reqfileid, '')       AS "reqfileid!"
              FROM uploadsessions
             WHERE id = $1
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("SqlUploadSessionStore.Get: failed to select session with id={id}"),
            source,
        })?;

        row.map(UploadSession::from)
            .ok_or_else(|| StoreError::NotFound {
                entity: "UploadSession",
                criteria: format!("id={id}"),
            })
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    async fn get_for_user(&self, user_id: &str) -> Result<Vec<UploadSession>, StoreError> {
        let rows = sqlx::query_as!(
            UploadSessionRow,
            r#"
            SELECT id                            AS "id!",
                   COALESCE(type::text, '')      AS "type_!",
                   COALESCE(createat, 0)         AS "createat!",
                   COALESCE(userid, '')          AS "userid!",
                   COALESCE(channelid, '')       AS "channelid!",
                   COALESCE(filename, '')        AS "filename!",
                   COALESCE(path, '')            AS "path!",
                   COALESCE(filesize, 0)         AS "filesize!",
                   COALESCE(fileoffset, 0)       AS "fileoffset!",
                   COALESCE(remoteid, '')        AS "remoteid!",
                   COALESCE(reqfileid, '')       AS "reqfileid!"
              FROM uploadsessions
             WHERE userid = $1
             ORDER BY createat ASC
            "#,
            user_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "SqlUploadSessionStore.GetForUser: failed to select".to_owned(),
            source,
        })?;

        Ok(rows.into_iter().map(UploadSession::from).collect())
    }
}
