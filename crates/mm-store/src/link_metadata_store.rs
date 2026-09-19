//! Port of `SqlLinkMetadataStore` (channels/store/sqlstore/link_metadata_store.go) — the table
//! behind link previews.
//!
//! `LinkMetadata` is a cache the two servers **share**: a row is keyed by
//! `GenerateLinkMetadataHash(url, hour)` and holds what one fetch of that URL found in that
//! hour — an OpenGraph document, an image's dimensions, or `none`. `Data` is `jsonb` since
//! migration 000061, so key order inside it is Postgres's, not either writer's.
//!
//! Go writes `json.Marshal(metadata.Data)`, so a `none` row holds the JSON **`null`**, not SQL
//! `NULL`; binding `serde_json::Value::Null` reproduces that. `IsBinaryParamEnabled` (the
//! `binary_parameters=yes` DSN option) is not modelled: without it Go binds the bytes as text.

use sqlx::PgPool;

use mm_model::link_metadata::LinkMetadata;

use crate::error::StoreError;

/// Port of `store.LinkMetadataStore`.
pub trait LinkMetadataStore {
    /// Port of `SqlLinkMetadataStore.Save` (link_metadata_store.go:47): `IsValid`, `PreSave`,
    /// then an insert that **upserts on the hash** — rewriting the URL, timestamp, type and data
    /// — so two servers fetching the same link in the same hour leave one row, the later one's.
    /// The validation failure is [`StoreError::Invalid`] carrying Go's `AppError`.
    fn save(
        &self,
        metadata: &mut LinkMetadata,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlLinkMetadataStore.Get` (link_metadata_store.go:82): the row for exactly this
    /// URL and timestamp — **not** the hash, so a hash collision between two URLs is a miss —
    /// or [`StoreError::NotFound`] keyed `LinkMetadata`. `Data` comes back as the stored JSON;
    /// turning it into a concrete type (`DeserializeDataToConcreteType`) is the caller's.
    fn get(
        &self,
        url: &str,
        timestamp: i64,
    ) -> impl std::future::Future<Output = Result<LinkMetadata, StoreError>> + Send;
}

#[derive(Debug, Clone)]
pub struct SqlLinkMetadataStore {
    pool: PgPool,
}

impl SqlLinkMetadataStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl LinkMetadataStore for SqlLinkMetadataStore {
    #[tracing::instrument(skip_all, fields(url = %metadata.url, timestamp = metadata.timestamp))]
    async fn save(&self, metadata: &mut LinkMetadata) -> Result<(), StoreError> {
        metadata
            .is_valid()
            .map_err(|app_error| StoreError::Invalid {
                entity: "LinkMetadata",
                app_error,
            })?;
        metadata.pre_save();
        // `json.Marshal(metadata.Data)` — a nil interface is the four bytes `null`.
        let data = metadata.data.clone().unwrap_or(serde_json::Value::Null);
        sqlx::query!(
            r#"INSERT INTO linkmetadata (hash, url, "timestamp", type, data)
               VALUES ($1, $2, $3, $4, $5)
               ON CONFLICT (hash) DO UPDATE
                 SET url = $2, "timestamp" = $3, type = $4, data = $5"#,
            metadata.hash,
            metadata.url,
            metadata.timestamp,
            metadata.link_type,
            data,
        )
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|source| StoreError::Db {
            context: "could not save link metadata".to_owned(),
            source,
        })
    }

    #[tracing::instrument(skip(self))]
    async fn get(&self, url: &str, timestamp: i64) -> Result<LinkMetadata, StoreError> {
        let row = sqlx::query!(
            r#"SELECT hash AS "hash!", url AS "url?", "timestamp" AS "timestamp?",
                      type AS "type_?", data AS "data?"
                 FROM linkmetadata
                WHERE url = $1 AND "timestamp" = $2"#,
            url,
            timestamp,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("could not get metadata with selectone: url={url}"),
            source,
        })?;
        let Some(row) = row else {
            return Err(StoreError::NotFound {
                entity: "LinkMetadata",
                criteria: format!("url={url}"),
            });
        };
        Ok(LinkMetadata {
            hash: row.hash,
            url: row.url.unwrap_or_default(),
            timestamp: row.timestamp.unwrap_or_default(),
            link_type: row.type_.unwrap_or_default(),
            // SQL NULL and JSON `null` both scan to Go's nil `[]byte`/`"null"`, which
            // `DeserializeDataToConcreteType` turns into no data.
            data: row.data.filter(|data| !data.is_null()),
        })
    }
}
