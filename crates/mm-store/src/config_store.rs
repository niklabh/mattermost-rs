//! Port of `config.DatabaseStore` (server/config/database.go), the read half only.
//!
//! # Why a config *store* exists at all
//!
//! Every other value this server answers with comes from a table the Go server also writes, and
//! that shared database is the whole strangler-fig mechanism. Configuration was the one
//! exception: Go's default backing store is `config.FileStore` over `config.json`, which lives on
//! a Docker volume this process cannot see, so the two servers shared a database but *not* a
//! configuration. Every ported permission check that consults a setting was reading an assumption
//! rather than a value — that was [D-156].
//!
//! Pointing `MM_CONFIG` at the shared Postgres DSN makes `config.IsDatabaseDSN` select
//! `config.DatabaseStore` instead (config/store.go:91), which keeps the whole `model.Config` as
//! one JSON document in `Configurations.Value`. This store reads that document. See the comment
//! on `MM_CONFIG` in `docker-compose.yml`.
//!
//! # The blob is the configuration Go *persists*, not the one it *runs on*
//!
//! `Store.Load` keeps two configs and writes back the wrong-looking one:
//!
//! ```text
//! loadedCfg      = SetDefaults() then applyEnvironmentMap()   -> s.config,      what Go runs on
//! loadedCfgNoEnv = SetDefaults() only                          -> persisted     (store.go:321)
//! ```
//!
//! So `MM_SERVICESETTINGS_SITEURL=http://localhost:8065` is live on the Go server and the blob
//! still says `""`. Measured, not inferred: with the compose stack's environment applied, the
//! persisted row carries `ServiceSettings.SiteURL == ""` and `TeamSettings.EnableOpenServer ==
//! false` while the running server has both set.
//!
//! **A reader of this row must therefore re-apply the environment overlay on top of it**, in that
//! order, or it will disagree with the server it sits beside on precisely the settings an
//! operator bothered to change. `mm_app::config::Config::load` is what does that.
//!
//! # `FeatureFlags` is not in the row
//!
//! `Store.Load` clears the section on all three configs before comparing or persisting when
//! `readOnlyFF` is set (store.go:306-310), which is the default. The persisted document has **no
//! `FeatureFlags` key at all** — confirmed against the live row. A feature flag can only ever come
//! from the environment or from a compiled-in default, never from this store, and code that reads
//! one must not "fall back to the database" for it.

use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `config.BackingStore` (config/store.go) that is ported.
pub trait ConfigStore {
    /// Port of `DatabaseStore.Load` (config/database.go:223).
    ///
    /// Returns the active configuration document verbatim, or `None` when no row is active.
    /// Go's own `Load` turns that same absence into a marshalled default `model.Config`; the
    /// `None` is kept here so the caller can tell "no configuration has ever been written" from
    /// "a configuration that happens to equal the defaults", which are different situations for a
    /// server that is about to answer with one of them.
    fn load_active(
        &self,
    ) -> impl std::future::Future<Output = Result<Option<String>, StoreError>> + Send;

    /// The `Id` of the active configuration row, or `None` when no row is active.
    ///
    /// Not a Go function: it is how this process notices that the document changed without
    /// reading and parsing the document. `DatabaseStore.persist` (config/database.go:161) writes
    /// every change as a **new row with a fresh `model.NewId()`** and skips the write altogether
    /// when the SHA is unchanged, so a different id is exactly "a different document" and an
    /// equal id is exactly "the same one". See `mm_app::App::refresh_config`.
    fn active_id(
        &self,
    ) -> impl std::future::Future<Output = Result<Option<String>, StoreError>> + Send;

    /// Port of `DatabaseStore.HasFile` (config/database.go:290): whether a configuration file of
    /// that name was persisted — `SELECT COUNT(*) FROM ConfigurationFiles WHERE Name = ?`, then
    /// `count != 0`.
    ///
    /// The name is whatever the configuration holds, and the default of every certificate-file
    /// setting is `""`, so the empty name is a real query that finds nothing rather than a
    /// short-circuit — Go makes no such check either. `SetFile`, `GetFile` and `RemoveFile` are
    /// not ported: every route that would call them writes the configuration document too, and
    /// forwards to Go before either write (see `mm_app::auth_certs`).
    fn has_file(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Result<bool, StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlConfigStore {
    pool: PgPool,
}

impl SqlConfigStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl ConfigStore for SqlConfigStore {
    /// `WHERE active` reproduces Go's predicate exactly (config/database.go:226).
    ///
    /// `active` is a *nullable* boolean carrying a UNIQUE constraint, which is how Go enforces
    /// "at most one active configuration": deactivating sets it to NULL rather than false
    /// (`UPDATE Configurations SET Active = NULL WHERE Active`, database.go:199), and Postgres
    /// permits many NULLs under a UNIQUE index but only one `true`. So the bare `WHERE active`
    /// is not shorthand for `active = true` being tidy — writing `active IS NOT FALSE` or
    /// `COALESCE(active, false)` would match every superseded revision instead of the current one.
    #[tracing::instrument(skip_all, fields(found, bytes))]
    async fn load_active(&self) -> Result<Option<String>, StoreError> {
        let value = sqlx::query_scalar!("SELECT value FROM configurations WHERE active")
            .fetch_optional(&self.pool)
            .await
            .map_err(|source| StoreError::Db {
                context: "failed to load the active configuration".to_owned(),
                source,
            })?;

        tracing::Span::current().record("found", value.is_some());
        tracing::Span::current().record("bytes", value.as_deref().map_or(0, str::len));
        Ok(value)
    }

    /// The same `WHERE active` as [`ConfigStore::load_active`], for the same reason.
    #[tracing::instrument(skip_all, fields(found))]
    async fn active_id(&self) -> Result<Option<String>, StoreError> {
        let id = sqlx::query_scalar!("SELECT id FROM configurations WHERE active")
            .fetch_optional(&self.pool)
            .await
            .map_err(|source| StoreError::Db {
                context: "failed to query the active configuration id".to_owned(),
                source,
            })?;
        tracing::Span::current().record("found", id.is_some());
        Ok(id)
    }

    /// `COUNT(*)` scanned into an `int64`, compared with zero (database.go:302). `query_scalar!`
    /// types the aggregate as nullable because Postgres does not promise otherwise; a `NULL` here
    /// would be a count of nothing, which is what the `unwrap_or(0)` says.
    #[tracing::instrument(skip_all, fields(name = %name, found))]
    async fn has_file(&self, name: &str) -> Result<bool, StoreError> {
        let count = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM configurationfiles WHERE name = $1",
            name
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to scan count of rows for {name}"),
            source,
        })?;
        let found = count.unwrap_or(0) != 0;
        tracing::Span::current().record("found", found);
        Ok(found)
    }
}
