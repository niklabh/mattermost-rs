//! Port of `SqlTemporaryPostStore` (channels/store/sqlstore/temporary_post_store.go), narrowed
//! to the get and the save `revealPost` reaches.
//!
//! `TemporaryPosts` holds the **content** of a burn-on-read post — the message and the file ids
//! — keyed by the post's id, while the `Posts` row carries an empty message and `[]` files. A
//! reveal copies the content back onto a clone of the post; the purge job deletes the row at
//! `ExpireAt`. `FileIds` is a JSON array in a varchar, as `Posts.FileIds` is.
//!
//! Go's `allowFromCache` parameter selects a per-node LRU in front of this; there is no cache
//! here, so every read is the row.

use sqlx::PgPool;

use mm_model::temporary_post::TemporaryPost;
use mm_model::utils::StringArray;

use crate::error::StoreError;

/// Port of `store.TemporaryPostStore`, narrowed to the two methods the reveal reaches.
pub trait TemporaryPostStore {
    /// Port of `SqlTemporaryPostStore.Get` (temporary_post_store.go:95): the row, or
    /// [`StoreError::NotFound`] keyed `TemporaryPost`. A `FileIds` column that is not a JSON
    /// array is a decode error, as `json.Unmarshal` makes it.
    fn get(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<TemporaryPost, StoreError>> + Send;

    /// Port of `SqlTemporaryPostStore.Save` (temporary_post_store.go:51): `IsValid`, then an
    /// upsert on the post id that rewrites all four other columns.
    fn save(
        &self,
        post: &TemporaryPost,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;
}

#[derive(Debug, Clone)]
pub struct SqlTemporaryPostStore {
    pool: PgPool,
}

impl SqlTemporaryPostStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl TemporaryPostStore for SqlTemporaryPostStore {
    #[tracing::instrument(skip(self), fields(post_id = %id))]
    async fn get(&self, id: &str) -> Result<TemporaryPost, StoreError> {
        let row = sqlx::query!(
            r#"SELECT postid AS "post_id!", type AS "type_!", expireat AS "expire_at!",
                      message AS "message?", fileids AS "file_ids?"
                 FROM temporaryposts
                WHERE postid = $1"#,
            id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get TemporaryPost with id={id}"),
            source,
        })?;

        let Some(row) = row else {
            return Err(StoreError::NotFound {
                entity: "TemporaryPost",
                criteria: id.to_owned(),
            });
        };

        // `json.Unmarshal([]byte(row.FileIDs), &fileIds)` on a nullable column: Go scans NULL
        // into an empty string, and unmarshalling `""` is an error — the same shape a corrupt
        // column produces. A JSON `null` decodes to a nil slice, which marshals as `null`.
        let file_ids: Option<StringArray> =
            serde_json::from_str(row.file_ids.as_deref().unwrap_or("")).map_err(|source| {
                StoreError::Decode {
                    entity: "TemporaryPost",
                    column: "fileids",
                    source,
                }
            })?;

        Ok(TemporaryPost {
            id: row.post_id,
            type_: row.type_,
            expire_at: row.expire_at,
            message: row.message.unwrap_or_default(),
            file_ids,
        })
    }

    #[tracing::instrument(skip(self, post), fields(post_id = %post.id, expire_at = post.expire_at))]
    async fn save(&self, post: &TemporaryPost) -> Result<(), StoreError> {
        // Go's bare `error`, which the app layer turns into the same 500 as a driver failure.
        if post.is_valid().is_err() {
            return Err(StoreError::Argument {
                entity: "TemporaryPost",
                detail: "failed to save TemporaryPost: id is required",
            });
        }
        // `model.ArrayToJSON(post.FileIDs)`: a nil slice is `[]`, never `null`.
        let file_ids =
            serde_json::to_string(post.file_ids.as_deref().unwrap_or(&[])).map_err(|source| {
                StoreError::Decode {
                    entity: "TemporaryPost",
                    column: "fileids",
                    source,
                }
            })?;
        sqlx::query!(
            r#"INSERT INTO temporaryposts (postid, type, expireat, message, fileids)
               VALUES ($1, $2, $3, $4, $5)
               ON CONFLICT (postid) DO UPDATE
                 SET type = $2, expireat = $3, message = $4, fileids = $5"#,
            post.id,
            post.type_,
            post.expire_at,
            post.message,
            file_ids,
        )
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|source| StoreError::Db {
            context: "failed to save TemporaryPost".to_owned(),
            source,
        })
    }
}
