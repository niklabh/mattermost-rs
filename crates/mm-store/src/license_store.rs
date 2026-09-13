//! Port of `SqlLicenseStore` (channels/store/sqlstore/license_store.go), `Get` only.
//!
//! `Get` is what `PlatformService.LoadLicense` (platform/license.go:104) reads once it has the
//! id from `Systems.ActiveLicenseId`, and it is the only read a request path needs. `Save` and
//! `GetAll` belong to `addLicense` and the licence-history endpoints, and land with those routes.

use mm_model::license::LicenseRecord;
use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `store.LicenseStore` (store/store.go:726) that is ported.
pub trait LicenseStore {
    /// Port of `SqlLicenseStore.Get` (license_store.go:56).
    ///
    /// Go turns **every** failure here into `ErrNotFound`, a driver error included, and
    /// `LoadLicense` reads that as "no licence". Copied per request that would answer an
    /// unlicensed installation for a licensed one whenever the database hiccuped — the same
    /// argument `mm_app::license` makes for the `ActiveLicenseId` read — so a driver failure is
    /// [`StoreError::Db`] and only a genuinely absent row is `None`.
    fn get(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<Option<LicenseRecord>, StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlLicenseStore {
    pool: PgPool,
}

impl SqlLicenseStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl LicenseStore for SqlLicenseStore {
    /// `createat` and `bytes` are nullable in the schema (000019_create_licenses.up.sql) while Go
    /// scans them into `int64` and `string`; `COALESCE` reproduces the zero values Go's scan
    /// would produce. A NULL `bytes` then fails validation exactly as an empty string does.
    #[tracing::instrument(skip(self), fields(id = id, found))]
    async fn get(&self, id: &str) -> Result<Option<LicenseRecord>, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT id                    AS "id!",
                   COALESCE(createat, 0) AS "createat!",
                   COALESCE(bytes, '')   AS "bytes!"
              FROM licenses
             WHERE id = $1
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find License with id={id}"),
            source,
        })?;

        tracing::Span::current().record("found", row.is_some());
        Ok(row.map(|row| LicenseRecord {
            id: row.id,
            create_at: row.createat,
            bytes: row.bytes,
        }))
    }
}
