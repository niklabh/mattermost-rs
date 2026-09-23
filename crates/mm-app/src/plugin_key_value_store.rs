//! Port of app/plugin_key_value_store.go and the plugin half of platform/cluster.go: the plugin
//! key-value store as the plugin API's `KV*` methods see it (docs/PLUGIN_PLAN.md, Phase 6).
//!
//! # Keys written before 5.6 were hashed
//!
//! Old servers stored `base64(sha256(key))` rather than the key ([`key_hash`]). So a read that
//! misses tries the hashed spelling too, every write and compare-and-delete then removes the
//! hashed row (a failure there is only a warning), and a delete removes **both**, hashed first.
//! A hashed row is still a key like any other to `KVList`, which lists it under its hash.
//!
//! # Which error a caller sees
//!
//! A validation failure crosses as the model's own 400 (`errors.As` finds the `*AppError` the
//! store returned); anything else is the operation's 500 with the driver error wrapped. A read
//! that finds nothing, under either spelling, is `Ok(None)`.

use base64::Engine as _;
use mm_model::plugin_key_value::PluginKeyValue;
use mm_model::plugin_kvset_options::PluginKVSetOptions;
use mm_model::utils::AppError;
use mm_store::{PluginStore as _, StoreError};
use sha2::{Digest as _, Sha256};

use crate::App;

/// Port of `getKeyHash` (app/plugin_key_value_store.go:18): how a key was stored before 5.6.
pub fn key_hash(key: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(key.as_bytes()))
}

/// The store's validation error as it was, or `id` as a 500 around the driver failure.
fn store_error(err: StoreError, r#where: &str, id: &str) -> Box<AppError> {
    match err {
        StoreError::Invalid { app_error, .. } => app_error,
        other => Box::new(AppError::new(r#where, id, None, "", 500).wrap(other)),
    }
}

impl App {
    /// Port of `App.SetPluginKey` (app/plugin_key_value_store.go:24).
    pub async fn set_plugin_key(
        &self,
        plugin_id: &str,
        key: &str,
        value: Option<&[u8]>,
    ) -> Result<(), Box<AppError>> {
        self.set_plugin_key_with_expiry(plugin_id, key, value, 0)
            .await
    }

    /// Port of `App.SetPluginKeyWithExpiry` (app/plugin_key_value_store.go:28). A negative expiry
    /// writes a key that is already expired.
    pub async fn set_plugin_key_with_expiry(
        &self,
        plugin_id: &str,
        key: &str,
        value: Option<&[u8]>,
        expire_in_seconds: i64,
    ) -> Result<(), Box<AppError>> {
        let options = PluginKVSetOptions {
            expire_in_seconds,
            ..PluginKVSetOptions::default()
        };
        self.set_plugin_key_with_options(plugin_id, key, value, &options)
            .await
            .map(|_| ())
    }

    /// Port of `App.CompareAndSetPluginKey` (app/plugin_key_value_store.go:36). A `None` old
    /// value inserts only when the key holds nothing live.
    pub async fn compare_and_set_plugin_key(
        &self,
        plugin_id: &str,
        key: &str,
        old_value: Option<&[u8]>,
        new_value: Option<&[u8]>,
    ) -> Result<bool, Box<AppError>> {
        let options = PluginKVSetOptions {
            atomic: true,
            old_value: old_value.map(<[u8]>::to_vec),
            expire_in_seconds: 0,
        };
        self.set_plugin_key_with_options(plugin_id, key, new_value, &options)
            .await
    }

    /// Port of `PlatformService.SetPluginKeyWithOptions` (platform/cluster.go:91).
    #[tracing::instrument(skip(self, value, options))]
    pub async fn set_plugin_key_with_options(
        &self,
        plugin_id: &str,
        key: &str,
        value: Option<&[u8]>,
        options: &PluginKVSetOptions,
    ) -> Result<bool, Box<AppError>> {
        if let Err(err) = options.is_valid() {
            tracing::debug!(plugin_id, key, error = %err, "Failed to set plugin key value with options");
            return Err(err);
        }
        let store = self.store().plugin();
        let updated = store
            .set_with_options(plugin_id, key, value, options)
            .await
            .map_err(|err| {
                tracing::error!(plugin_id, key, error = %err, "Failed to set plugin key value with options");
                store_error(
                    err,
                    "SetPluginKeyWithOptions",
                    "app.plugin_store.save.app_error",
                )
            })?;
        if let Err(err) = store.delete(plugin_id, &key_hash(key)).await {
            tracing::warn!(plugin_id, key, error = %err, "Failed to clean up previously hashed plugin key value");
        }
        Ok(updated)
    }

    /// Port of `App.CompareAndDeletePluginKey` (app/plugin_key_value_store.go:48).
    ///
    /// Go answers `deleted, appErr` on a validation failure, which is always `false`: the store
    /// validates before it deletes anything.
    #[tracing::instrument(skip(self, old_value))]
    pub async fn compare_and_delete_plugin_key(
        &self,
        plugin_id: &str,
        key: &str,
        old_value: Option<&[u8]>,
    ) -> Result<bool, Box<AppError>> {
        let kv = PluginKeyValue {
            plugin_id: plugin_id.to_owned(),
            key: key.to_owned(),
            value: None,
            expire_at: 0,
        };
        let store = self.store().plugin();
        let deleted = store
            .compare_and_delete(&kv, old_value)
            .await
            .map_err(|err| {
                tracing::error!(key, error = %err, "Failed to compare and delete plugin key value");
                store_error(
                    err,
                    "CompareAndDeletePluginKey",
                    "app.plugin_store.delete.app_error",
                )
            })?;
        if let Err(err) = store.delete(plugin_id, &key_hash(key)).await {
            tracing::warn!(key, error = %err, "Failed to clean up previously hashed plugin key value");
        }
        Ok(deleted)
    }

    /// Port of `App.GetPluginKey` (app/plugin_key_value_store.go:95, `Server.getPluginKey`): the
    /// key, then its pre-5.6 hashed spelling, then nothing.
    #[tracing::instrument(skip(self))]
    pub async fn get_plugin_key(
        &self,
        plugin_id: &str,
        key: &str,
    ) -> Result<Option<Vec<u8>>, Box<AppError>> {
        let store = self.store().plugin();
        for (spelling, what) in [
            (key.to_owned(), "Failed to query plugin key value"),
            (
                key_hash(key),
                "Failed to query plugin key value using hashed key",
            ),
        ] {
            match store.get(plugin_id, &spelling).await {
                Ok(kv) => return Ok(kv.value),
                Err(StoreError::NotFound { .. }) => {}
                Err(err) => {
                    tracing::error!(plugin_id, key, error = %err, "{what}");
                    return Err(store_error(
                        err,
                        "GetPluginKey",
                        "app.plugin_store.get.app_error",
                    ));
                }
            }
        }
        Ok(None)
    }

    /// Port of `PlatformService.DeletePluginKey` (platform/cluster.go:246): the hashed spelling
    /// first, then the key. A key that is not there is not an error.
    #[tracing::instrument(skip(self))]
    pub async fn delete_plugin_key(&self, plugin_id: &str, key: &str) -> Result<(), Box<AppError>> {
        let store = self.store().plugin();
        for (spelling, what) in [
            (key_hash(key), "Failed to delete plugin key value"),
            (
                key.to_owned(),
                "Failed to delete plugin key value using hashed key",
            ),
        ] {
            if let Err(err) = store.delete(plugin_id, &spelling).await {
                tracing::error!(plugin_id, key, error = %err, "{what}");
                return Err(store_error(
                    err,
                    "DeletePluginKey",
                    "app.plugin_store.delete.app_error",
                ));
            }
        }
        Ok(())
    }

    /// Port of `App.DeleteAllKeysForPlugin` (app/plugin_key_value_store.go:107): every key of
    /// this plugin, expired ones included, and no other plugin's.
    #[tracing::instrument(skip(self))]
    pub async fn delete_all_keys_for_plugin(&self, plugin_id: &str) -> Result<(), Box<AppError>> {
        self.store()
            .plugin()
            .delete_all_for_plugin(plugin_id)
            .await
            .map_err(|err| {
                tracing::error!(plugin_id, error = %err, "Failed to delete all plugin key values");
                store_error(
                    err,
                    "DeleteAllKeysForPlugin",
                    "app.plugin_store.delete.app_error",
                )
            })
    }

    /// Port of `PlatformService.ListPluginKeys` (platform/cluster.go:236): page `page` of
    /// `per_page` live keys. The offset is `page * per_page` with Go's `int` wrap-around, and the
    /// store then clamps it and the page size (see `mm_store::plugin_store`).
    #[tracing::instrument(skip(self))]
    pub async fn list_plugin_keys(
        &self,
        plugin_id: &str,
        page: i64,
        per_page: i64,
    ) -> Result<Vec<String>, Box<AppError>> {
        self.store()
            .plugin()
            .list(plugin_id, page.wrapping_mul(per_page), per_page)
            .await
            .map_err(|err| {
                tracing::error!(page, per_page, error = %err, "Failed to list plugin key values");
                store_error(err, "ListPluginKeys", "app.plugin_store.list.app_error")
            })
    }

    /// Port of `PlatformService.GetSystemInstallDate` (platform/config.go:404): the
    /// `InstallationDate` system row, parsed. A missing row is the lookup's 500, as Go's store
    /// answers `ErrNotFound` for one.
    pub async fn get_system_install_date(&self) -> Result<i64, Box<AppError>> {
        use mm_store::SystemStore as _;
        let lookup = |detail: Option<StoreError>| {
            let err = AppError::new(
                "getSystemInstallDate",
                "app.system.get_by_name.app_error",
                None,
                "",
                500,
            );
            Box::new(match detail {
                Some(source) => err.wrap(source),
                None => err,
            })
        };
        let value = match self
            .store()
            .system()
            .get_by_name(mm_model::system::SYSTEM_INSTALLATION_DATE_KEY)
            .await
        {
            Ok(Some(value)) => value,
            Ok(None) => return Err(lookup(None)),
            Err(err) => return Err(lookup(Some(err))),
        };
        value.parse::<i64>().map_err(|err| {
            Box::new(
                AppError::new(
                    "getSystemInstallDate",
                    "app.system_install_date.parse_int.app_error",
                    None,
                    "",
                    500,
                )
                .wrap(err),
            )
        })
    }

    /// Port of `Server.ServerId` (app/server.go:1826): the `DiagnosticId` system row, or the empty
    /// string when it cannot be read.
    pub async fn server_id(&self) -> String {
        self.telemetry_id().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `getKeyHash` is standard base64, padded, of the SHA-256 — values from Go's
    /// `base64.StdEncoding.EncodeToString(sha256.Sum256(...))`.
    #[test]
    fn a_key_hashes_as_go_hashed_it_before_5_6() {
        assert_eq!(key_hash(""), "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=");
        assert_eq!(
            key_hash("abc"),
            "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0="
        );
        assert_eq!(key_hash("abc").len(), 44, "fits the 150-character key");
    }

    /// A validation failure is the model's own error; anything else is the operation's 500.
    #[test]
    fn a_store_error_maps_as_errors_as_does() {
        let invalid = StoreError::Invalid {
            entity: "PluginKeyValue",
            app_error: Box::new(AppError::new("W", "model.x", None, "key=", 400)),
        };
        let err = store_error(invalid, "GetPluginKey", "app.plugin_store.get.app_error");
        assert_eq!((err.id.as_str(), err.status_code), ("model.x", 400));

        let db = StoreError::Db {
            context: "failed".into(),
            source: sqlx::Error::PoolTimedOut,
        };
        let err = store_error(db, "ListPluginKeys", "app.plugin_store.list.app_error");
        assert_eq!(
            (err.id.as_str(), err.where_.as_str(), err.status_code),
            ("app.plugin_store.list.app_error", "ListPluginKeys", 500)
        );
        assert_eq!(
            err.detailed_error, "",
            "the driver error is wrapped, not detailed"
        );
    }
}
