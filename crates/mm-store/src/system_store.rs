//! Port of `SqlSystemStore` (channels/store/sqlstore/system_store.go), `GetByName` only.
//!
//! Ported for `getClientLicense` (api4/license.go:31), which needs to know whether this
//! installation has a licence at all — `PlatformService.LoadLicense` (app/platform/license.go:49)
//! reads the id from `Systems` under [`SYSTEM_ACTIVE_LICENSE_ID`] and looks the licence up by it.

use sqlx::PgPool;

use crate::error::StoreError;

/// `model.SystemActiveLicenseId` (model/system.go:14).
///
/// The row Go writes when a licence is installed and blanks when one is removed, so it — not the
/// presence of a row in `Licenses` — is what says whether a server is licensed.
pub const SYSTEM_ACTIVE_LICENSE_ID: &str = "ActiveLicenseId";

/// The subset of Go's `store.SystemStore` (store/store.go) that is ported.
pub trait SystemStore {
    /// Port of `SqlSystemStore.GetByName` (system_store.go:88).
    ///
    /// Go returns `*model.System`, and its `ErrNoRows` branch becomes a typed `ErrNotFound` that
    /// every caller turns straight back into "absent". `Option` says the same thing without the
    /// error, and the value is all any migrated caller reads — which is also why `model.System`
    /// itself is not ported: it is not on the wire for any route this server answers, and a model
    /// type nothing serialises is a guess about a JSON shape nothing can falsify.
    fn get_by_name(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Result<Option<String>, StoreError>> + Send;

    /// Port of `SqlSystemStore.PermanentDeleteByName` (system_store.go:105) — one `DELETE`.
    ///
    /// Go's signature returns `*model.System`, and the struct it returns is the **zero value**:
    /// it declares `var system model.System`, never reads a row into it, and returns `&system`.
    /// Even its own error message interpolates `system.Name`, which is always `""`. Nothing
    /// reachable reads the result, so this returns `()` rather than reproducing an empty struct.
    fn permanent_delete_by_name(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlSystemStore {
    pool: PgPool,
}

impl SqlSystemStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl SystemStore for SqlSystemStore {
    #[tracing::instrument(skip(self), fields(name = name))]
    async fn permanent_delete_by_name(&self, name: &str) -> Result<(), StoreError> {
        sqlx::query!("DELETE FROM systems WHERE name = $1", name)
            .execute(&self.pool)
            .await
            .map_err(|source| StoreError::Db {
                // Go interpolates `system.Name` here, which is the zero value — so its message
                // never names the row it failed on. Ours does.
                context: format!("failed to permanent delete system property with name={name}"),
                source,
            })?;
        Ok(())
    }

    /// `Value` is nullable in the schema while Go scans it into a plain `string`, so a NULL there
    /// fails Go's own scan and the row is unreadable to both servers. `COALESCE` is *not* applied:
    /// mirroring the column's nullability keeps a NULL distinguishable from an empty string, and
    /// the caller treats both as "no licence" anyway.
    #[tracing::instrument(skip_all, fields(name = name, found))]
    async fn get_by_name(&self, name: &str) -> Result<Option<String>, StoreError> {
        let value = sqlx::query_scalar!("SELECT value FROM systems WHERE name = $1", name)
            .fetch_optional(&self.pool)
            .await
            .map_err(|source| StoreError::Db {
                context: format!("failed to get system property with name={name}"),
                source,
            })?
            .flatten();

        tracing::Span::current().record("found", value.is_some());
        Ok(value)
    }
}
