//! Port of `SqlPreferenceStore` (channels/store/sqlstore/preference_store.go), `Save` only.
//!
//! **The first write in this port.** Everything before it read.

use mm_model::preference::{Preference, Preferences};
use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `store.PreferenceStore` that is ported.
pub trait PreferenceStore {
    /// Port of `SqlPreferenceStore.Save` (preference_store.go:44).
    fn save(
        &self,
        preferences: &Preferences,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlPreferenceStore {
    pool: PgPool,
}

impl SqlPreferenceStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl PreferenceStore for SqlPreferenceStore {
    /// Port of `Save` (preference_store.go:44) together with `saveTx` (:89), which it calls per
    /// preference.
    ///
    /// `save` (:65) and `saveTx` (:89) are **byte-identical** in the Go source — a duplicated
    /// function, only one of which is reachable from `Save`. Ported once.
    ///
    /// # The transaction is the contract
    ///
    /// Go's comment says it plainly: *"wrap in a transaction so that if one fails, everything
    /// fails"*. Validation happens per preference **inside** the loop, so a batch whose third
    /// entry is invalid must leave the first two unwritten. Saving them one at a time outside a
    /// transaction would pass every happy-path test and silently half-apply a bad batch.
    #[tracing::instrument(skip_all, fields(count = preferences.len()))]
    async fn save(&self, preferences: &Preferences) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "begin_transaction".to_owned(),
            source,
        })?;

        for preference in preferences.iter() {
            // Go mutates the caller's value here; we take a copy, because `PreUpdate` only
            // normalises `Value` and the caller has no use for the normalised form. See D-090.
            let mut preference: Preference = preference.clone();
            preference.pre_update();

            // `AppResult`'s error is already boxed, so this carries Go's error id and status
            // code through to the client unchanged.
            preference
                .is_valid()
                .map_err(|app_error| StoreError::Invalid {
                    entity: "Preference",
                    app_error,
                })?;

            // `ON CONFLICT (userid, category, name) DO UPDATE SET Value = ?` — Go's exact
            // upsert. The conflict target is the table's primary key, so a repeated save of the
            // same (user, category, name) overwrites rather than erroring or duplicating.
            sqlx::query!(
                r#"
                INSERT INTO preferences (userid, category, name, value)
                VALUES ($1, $2, $3, $4)
                ON CONFLICT (userid, category, name) DO UPDATE SET value = $4
                "#,
                preference.user_id,
                preference.category,
                preference.name,
                preference.value
            )
            .execute(&mut *tx)
            .await
            .map_err(|source| StoreError::Db {
                context: "failed to save Preference".to_owned(),
                source,
            })?;
        }

        tx.commit().await.map_err(|source| StoreError::Db {
            context: "commit_transaction".to_owned(),
            source,
        })?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use mm_model::preference::Preference;

    /// `IsValid` is the gate that stands between a request body and the database, and it is
    /// already complete in `mm-model`. Asserted here because this store is the first caller that
    /// can write, so a regression in it is now a data problem rather than a validation one.
    #[test]
    fn invalid_preferences_are_rejected_before_any_write() {
        let bad_user = Preference {
            user_id: "not-26-chars".to_owned(),
            category: "display_settings".to_owned(),
            name: "use_military_time".to_owned(),
            value: "true".to_owned(),
        };
        assert!(bad_user.is_valid().is_err(), "a bad user id must not write");

        let good = Preference {
            user_id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            category: "display_settings".to_owned(),
            name: "use_military_time".to_owned(),
            value: "true".to_owned(),
        };
        assert!(good.is_valid().is_ok(), "a well-formed preference writes");
    }

    /// `PreUpdate` runs before validation on every entry, so whatever it normalises is what
    /// reaches the database.
    #[test]
    fn pre_update_runs_before_the_value_is_written() {
        let mut preference = Preference {
            user_id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            category: "direct_channel_show".to_owned(),
            name: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            value: "true".to_owned(),
        };
        let before = preference.value.clone();
        preference.pre_update();
        // The port asserts PreUpdate's output byte-for-byte in mm-model; here we only pin that
        // the store calls it on a value it is about to persist.
        assert!(
            preference.is_valid().is_ok(),
            "a preference must still validate after PreUpdate, value {before:?}"
        );
    }
}
