//! Port of `SqlPluginStore` (channels/store/sqlstore/plugin_store.go): the key-value store every
//! plugin gets through the plugin API's `KV*` methods (docs/PLUGIN_PLAN.md, Phase 6).
//!
//! # Expiry is a predicate, not a sweep
//!
//! A row whose `ExpireAt` is non-zero and in the past is still in the table until
//! `DeleteAllExpired` runs (a job this server does not schedule yet), so every read and every
//! conditional write carries `(ExpireAt = 0 OR ExpireAt > now)` itself. Two consequences a reader
//! would not guess:
//!
//! - **A NULL `ExpireAt` is invisible and immovable.** Migration 45 left the column's default at
//!   NULL, so a row written by a pre-6.x server can carry one. `NULL = 0` and `NULL > now` are
//!   both unknown, so no read finds it; `NULL <> 0 AND NULL < now` is unknown too, so the
//!   insert-if-absent path does not clear it, and the insert then hits the primary key and
//!   answers `false`.
//! - **An expired row blocks nothing on insert** — [`PluginStore::compare_and_set`] deletes it
//!   first — but **everything on update**: a compare against an expired value never matches.
//!
//! # nil and empty are one value here
//!
//! Go distinguishes a nil `[]byte` from an empty one (`oldValue == nil` picks the insert path), but
//! nothing that reaches this store can send the difference: gob omits an empty slice, so the host
//! decodes both as nil. `Option<&[u8]>` is kept anyway, so that `Some(&[])` would take Go's
//! non-nil branch for a caller that is not the RPC.

use mm_model::plugin_key_value::PluginKeyValue;
use mm_model::plugin_kvset_options::{PluginKVSetOptions, new_plugin_key_value_from_options};
use mm_model::utils::get_millis;
use sqlx::PgPool;

use crate::error::StoreError;

/// `defaultPluginKeyFetchLimit` (plugin_store.go:18): the page size [`PluginStore::list`] uses
/// when asked for none.
pub const DEFAULT_PLUGIN_KEY_FETCH_LIMIT: i64 = 10;

/// The names `CompareAndSet` passes to `IsUniqueConstraintError` (plugin_store.go:98). Go matches
/// them as substrings of the whole error text; for Postgres it is the constraint name,
/// `pluginkeyvaluestore_pkey`, that matches.
const UNIQUE_NAMES: [&str; 5] = ["PRIMARY", "PluginId", "Key", "PKey", "pkey"];

/// Port of `store.PluginStore` (store/store.go), less `DeleteAllExpired`, which only the expiry
/// job calls.
pub trait PluginStore {
    /// Port of `SqlPluginStore.SaveOrUpdate` (plugin_store.go:29). A `None` value deletes the key.
    fn save_or_update(
        &self,
        kv: &PluginKeyValue,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlPluginStore.CompareAndSet` (plugin_store.go:59).
    fn compare_and_set(
        &self,
        kv: &PluginKeyValue,
        old_value: Option<&[u8]>,
    ) -> impl std::future::Future<Output = Result<bool, StoreError>> + Send;

    /// Port of `SqlPluginStore.CompareAndDelete` (plugin_store.go:145).
    fn compare_and_delete(
        &self,
        kv: &PluginKeyValue,
        old_value: Option<&[u8]>,
    ) -> impl std::future::Future<Output = Result<bool, StoreError>> + Send;

    /// Port of `SqlPluginStore.SetWithOptions` (plugin_store.go:182).
    fn set_with_options(
        &self,
        plugin_id: &str,
        key: &str,
        value: Option<&[u8]>,
        opt: &PluginKVSetOptions,
    ) -> impl std::future::Future<Output = Result<bool, StoreError>> + Send;

    /// Port of `SqlPluginStore.Get` (plugin_store.go:203). An expired key is
    /// [`StoreError::NotFound`].
    fn get(
        &self,
        plugin_id: &str,
        key: &str,
    ) -> impl std::future::Future<Output = Result<PluginKeyValue, StoreError>> + Send;

    /// Port of `SqlPluginStore.Delete` (plugin_store.go:229). Nothing to delete is not an error.
    fn delete(
        &self,
        plugin_id: &str,
        key: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlPluginStore.DeleteAllForPlugin` (plugin_store.go:246): expired keys too.
    fn delete_all_for_plugin(
        &self,
        plugin_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlPluginStore.List` (plugin_store.go:280): the live keys, by key, `limit` from
    /// `offset`. A `limit` of zero or less is [`DEFAULT_PLUGIN_KEY_FETCH_LIMIT`]; a negative
    /// `offset` is zero.
    fn list(
        &self,
        plugin_id: &str,
        offset: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<String>, StoreError>> + Send;
}

#[derive(Debug, Clone)]
pub struct SqlPluginStore {
    pool: PgPool,
}

impl SqlPluginStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn invalid(app_error: Box<mm_model::utils::AppError>) -> StoreError {
    StoreError::Invalid {
        entity: "PluginKeyValue",
        app_error,
    }
}

fn db(context: impl Into<String>) -> impl FnOnce(sqlx::Error) -> StoreError {
    let context = context.into();
    move |source| StoreError::Db { context, source }
}

/// Port of `IsUniqueConstraintError(err, UNIQUE_NAMES)` (sqlstore/store.go:667): a `23505` whose
/// text names one of the key's parts.
fn is_key_conflict(err: &sqlx::Error) -> bool {
    err.as_database_error().is_some_and(|db| {
        db.code().as_deref() == Some("23505")
            && UNIQUE_NAMES.iter().any(|name| db.message().contains(name))
    })
}

impl PluginStore for SqlPluginStore {
    #[tracing::instrument(skip(self, kv), fields(plugin_id = %kv.plugin_id, key = %kv.key))]
    async fn save_or_update(&self, kv: &PluginKeyValue) -> Result<(), StoreError> {
        kv.is_valid().map_err(invalid)?;
        let Some(value) = kv.value.as_deref() else {
            // Setting a key to nil is the same as removing it.
            return self.delete(&kv.plugin_id, &kv.key).await;
        };
        sqlx::query!(
            "INSERT INTO PluginKeyValueStore (PluginId, PKey, PValue, ExpireAt) VALUES ($1, $2, $3, $4)
             ON CONFLICT (pluginid, pkey) DO UPDATE SET PValue = $5, ExpireAt = $6",
            kv.plugin_id,
            kv.key,
            value,
            kv.expire_at,
            value,
            kv.expire_at,
        )
        .execute(&self.pool)
        .await
        .map_err(db("failed to upsert PluginKeyValue"))?;
        Ok(())
    }

    #[tracing::instrument(skip(self, kv, old_value), fields(plugin_id = %kv.plugin_id, key = %kv.key))]
    async fn compare_and_set(
        &self,
        kv: &PluginKeyValue,
        old_value: Option<&[u8]>,
    ) -> Result<bool, StoreError> {
        kv.is_valid().map_err(invalid)?;
        let Some(value) = kv.value.as_deref() else {
            // Setting a key to nil is the same as removing it.
            return self.compare_and_delete(kv, old_value).await;
        };

        let Some(old_value) = old_value else {
            // Insert if absent: first clear an expired value, which would otherwise block it.
            sqlx::query!(
                "DELETE FROM PluginKeyValueStore
                 WHERE PluginId = $1 AND PKey = $2 AND ExpireAt <> 0 AND ExpireAt < $3",
                kv.plugin_id,
                kv.key,
                get_millis(),
            )
            .execute(&self.pool)
            .await
            .map_err(db("failed to delete PluginKeyValue"))?;

            let inserted = sqlx::query!(
                "INSERT INTO PluginKeyValueStore (PluginId, PKey, PValue, ExpireAt) VALUES ($1, $2, $3, $4)",
                kv.plugin_id,
                kv.key,
                value,
                kv.expire_at,
            )
            .execute(&self.pool)
            .await;
            return match inserted {
                Ok(_) => Ok(true),
                // Someone else holds the key: a lost race, or a live value. Not an error.
                Err(err) if is_key_conflict(&err) => Ok(false),
                Err(err) => Err(db("failed to insert PluginKeyValue")(err)),
            };
        };

        let updated = sqlx::query!(
            "UPDATE PluginKeyValueStore SET PValue = $1, ExpireAt = $2
             WHERE PluginId = $3 AND PKey = $4 AND PValue = $5 AND (ExpireAt = 0 OR ExpireAt > $6)",
            value,
            kv.expire_at,
            kv.plugin_id,
            kv.key,
            old_value,
            get_millis(),
        )
        .execute(&self.pool)
        .await
        .map_err(db("failed to update PluginKeyValue"))?;
        Ok(updated.rows_affected() != 0)
    }

    #[tracing::instrument(skip(self, kv, old_value), fields(plugin_id = %kv.plugin_id, key = %kv.key))]
    async fn compare_and_delete(
        &self,
        kv: &PluginKeyValue,
        old_value: Option<&[u8]>,
    ) -> Result<bool, StoreError> {
        kv.is_valid().map_err(invalid)?;
        let Some(old_value) = old_value else {
            // nil can't be stored, so nothing can match it.
            return Ok(false);
        };
        let deleted = sqlx::query!(
            "DELETE FROM PluginKeyValueStore
             WHERE PluginId = $1 AND PKey = $2 AND PValue = $3 AND (ExpireAt = 0 OR ExpireAt > $4)",
            kv.plugin_id,
            kv.key,
            old_value,
            get_millis(),
        )
        .execute(&self.pool)
        .await
        .map_err(db("failed to delete PluginKeyValue"))?;
        Ok(deleted.rows_affected() != 0)
    }

    #[tracing::instrument(skip(self, value, opt))]
    async fn set_with_options(
        &self,
        plugin_id: &str,
        key: &str,
        value: Option<&[u8]>,
        opt: &PluginKVSetOptions,
    ) -> Result<bool, StoreError> {
        opt.is_valid().map_err(invalid)?;
        let kv = new_plugin_key_value_from_options(plugin_id, key, value.map(<[u8]>::to_vec), opt);
        if opt.atomic {
            return self.compare_and_set(&kv, opt.old_value.as_deref()).await;
        }
        // Go answers `savedKv != nil`, which a successful `SaveOrUpdate` always is.
        self.save_or_update(&kv).await?;
        Ok(true)
    }

    #[tracing::instrument(skip(self))]
    async fn get(&self, plugin_id: &str, key: &str) -> Result<PluginKeyValue, StoreError> {
        let row = sqlx::query!(
            r#"SELECT PluginId AS "plugin_id!", PKey AS "key!", PValue AS value, ExpireAt AS "expire_at!"
               FROM PluginKeyValueStore
               WHERE PluginId = $1 AND PKey = $2 AND (ExpireAt = 0 OR ExpireAt > $3)"#,
            plugin_id,
            key,
            get_millis(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(db(format!(
            "failed to get PluginKeyValue with pluginId={plugin_id} and key={key}"
        )))?;
        let row = row.ok_or_else(|| StoreError::NotFound {
            entity: "PluginKeyValue",
            criteria: format!("pluginId={plugin_id}, key={key}"),
        })?;
        Ok(PluginKeyValue {
            plugin_id: row.plugin_id,
            key: row.key,
            value: row.value,
            expire_at: row.expire_at,
        })
    }

    #[tracing::instrument(skip(self))]
    async fn delete(&self, plugin_id: &str, key: &str) -> Result<(), StoreError> {
        sqlx::query!(
            "DELETE FROM PluginKeyValueStore WHERE PluginId = $1 AND PKey = $2",
            plugin_id,
            key,
        )
        .execute(&self.pool)
        .await
        .map_err(db(format!(
            "failed to delete PluginKeyValue with pluginId={plugin_id} and key={key}"
        )))?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn delete_all_for_plugin(&self, plugin_id: &str) -> Result<(), StoreError> {
        sqlx::query!(
            "DELETE FROM PluginKeyValueStore WHERE PluginId = $1",
            plugin_id,
        )
        .execute(&self.pool)
        .await
        .map_err(db(format!(
            "failed to get all PluginKeyValues with pluginId={plugin_id} "
        )))?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn list(
        &self,
        plugin_id: &str,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<String>, StoreError> {
        let limit = if limit <= 0 {
            DEFAULT_PLUGIN_KEY_FETCH_LIMIT
        } else {
            limit
        };
        let offset = offset.max(0);
        let keys = sqlx::query_scalar!(
            r#"SELECT PKey AS "key!" FROM PluginKeyValueStore
               WHERE PluginId = $1 AND (ExpireAt = 0 OR ExpireAt > $2)
               ORDER BY PKey LIMIT $3 OFFSET $4"#,
            plugin_id,
            get_millis(),
            limit,
            offset,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db(format!(
            "failed to get PluginKeyValues with pluginId={plugin_id}"
        )))?;
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unreachable_store() -> SqlPluginStore {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(200))
            .connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
            .unwrap();
        SqlPluginStore::new(pool)
    }

    fn kv(plugin_id: &str, key: &str, value: Option<&[u8]>) -> PluginKeyValue {
        PluginKeyValue {
            plugin_id: plugin_id.to_owned(),
            key: key.to_owned(),
            value: value.map(<[u8]>::to_vec),
            expire_at: 0,
        }
    }

    fn app_error_id(err: StoreError) -> String {
        match err {
            StoreError::Invalid { app_error, .. } => app_error.id,
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// Validation runs before any query, on every write path, so a store with no database
    /// answers the model's error rather than a driver one.
    #[tokio::test]
    async fn every_write_validates_before_it_queries() {
        let store = unreachable_store();
        let key = "model.plugin_key_value.is_valid.key.app_error";
        let bad = kv("p", "", Some(b"v"));
        assert_eq!(
            app_error_id(store.save_or_update(&bad).await.unwrap_err()),
            key
        );
        assert_eq!(
            app_error_id(store.compare_and_set(&bad, None).await.unwrap_err()),
            key
        );
        assert_eq!(
            app_error_id(
                store
                    .compare_and_delete(&bad, Some(b"v"))
                    .await
                    .unwrap_err()
            ),
            key
        );
        let no_plugin = kv("", "k", Some(b"v"));
        assert_eq!(
            app_error_id(store.save_or_update(&no_plugin).await.unwrap_err()),
            "model.plugin_key_value.is_valid.plugin_id.app_error"
        );
        let opt = PluginKVSetOptions {
            atomic: false,
            old_value: Some(b"x".to_vec()),
            expire_in_seconds: 0,
        };
        assert_eq!(
            app_error_id(
                store
                    .set_with_options("p", "k", Some(b"v"), &opt)
                    .await
                    .unwrap_err()
            ),
            "model.plugin_kvset_options.is_valid.old_value.app_error"
        );
    }

    /// A nil old value can match nothing, so the compare-and-delete answers `false` without a
    /// query — and a nil new value turns compare-and-set into that compare-and-delete.
    #[tokio::test]
    async fn a_nil_old_value_deletes_nothing_without_asking_the_database() {
        let store = unreachable_store();
        assert!(
            !store
                .compare_and_delete(&kv("p", "k", None), None)
                .await
                .unwrap()
        );
        assert!(
            !store
                .compare_and_set(&kv("p", "k", None), None)
                .await
                .unwrap()
        );
        assert!(
            store
                .compare_and_set(&kv("p", "k", None), Some(b"old"))
                .await
                .is_err(),
            "a non-nil old value does ask"
        );
    }
}
