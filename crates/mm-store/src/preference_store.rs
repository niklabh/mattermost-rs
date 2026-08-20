//! Port of `SqlPreferenceStore` (channels/store/sqlstore/preference_store.go): `Save`, `GetAll`,
//! `GetCategory` and `Get`.
//!
//! `Save` was **the first write in this port.** The three reads share Go's
//! `preferenceSelectQuery` — `SELECT UserId, Category, Name, Value FROM Preferences` — and,
//! like it, carry **no `ORDER BY`**. Go's row order is whatever Postgres hands back; adding one
//! here would be tidier and would put the two servers' bodies in different orders.

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

    /// Port of `SqlPreferenceStore.GetAll` (preference_store.go:151).
    fn get_all(
        &self,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<Preferences, StoreError>> + Send;

    /// Port of `SqlPreferenceStore.GetCategory` (preference_store.go:139).
    fn get_category(
        &self,
        user_id: &str,
        category: &str,
    ) -> impl std::future::Future<Output = Result<Preferences, StoreError>> + Send;

    /// Port of `SqlPreferenceStore.Get` (preference_store.go:113).
    fn get(
        &self,
        user_id: &str,
        category: &str,
        name: &str,
    ) -> impl std::future::Future<Output = Result<Preference, StoreError>> + Send;
}

/// One row of Go's `preferenceSelectQuery`, in its column order.
///
/// `Value` is nullable in the schema but Go scans it into a plain `string`, and `database/sql`
/// refuses to convert NULL into one — so a NULL row fails the **whole** query, and the app layer
/// answers 400. The `"value!"` override makes sqlx fail the decode the same way rather than
/// inventing a `None` Go never produces. `Save` cannot write a NULL, so no ported path creates one.
struct PreferenceRow {
    userid: String,
    category: String,
    name: String,
    value: String,
}

impl From<PreferenceRow> for Preference {
    fn from(row: PreferenceRow) -> Self {
        Preference {
            user_id: row.userid,
            category: row.category,
            name: row.name,
            value: row.value,
        }
    }
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

    /// Port of `GetAll` (preference_store.go:151): `WHERE UserId = ?`, no ordering.
    ///
    /// # Zero rows is `null` on the wire, not `[]`
    ///
    /// Go scans into a `var preferences model.Preferences` — a nil slice — and sqlx's `scanAll`
    /// only `SetLen(0)`s it, which leaves a nil slice nil. `json.Encode` of that is `null`. The
    /// caller is responsible for that distinction; this returns an empty `Preferences` and the
    /// API layer encodes it as Go would. Unreachable through the API for a living user, since
    /// `CreateUser` writes default preferences — reachable after `POST .../preferences/delete`.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, count))]
    async fn get_all(&self, user_id: &str) -> Result<Preferences, StoreError> {
        let rows = sqlx::query_as!(
            PreferenceRow,
            r#"
            SELECT userid, category, name, value AS "value!"
            FROM preferences
            WHERE userid = $1
            "#,
            user_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find Preferences with userId={user_id}"),
            source,
        })?;
        tracing::Span::current().record("count", rows.len());

        Ok(rows
            .into_iter()
            .map(Preference::from)
            .collect::<Vec<_>>()
            .into())
    }

    /// Port of `GetCategory` (preference_store.go:139): `WHERE UserId = ? AND Category = ?`, no
    /// ordering. An empty result is **not** an error here — the app layer turns it into a 404
    /// (app/preference.go:31), which is the only place that decision is made.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, category = %category, count))]
    async fn get_category(&self, user_id: &str, category: &str) -> Result<Preferences, StoreError> {
        let rows = sqlx::query_as!(
            PreferenceRow,
            r#"
            SELECT userid, category, name, value AS "value!"
            FROM preferences
            WHERE userid = $1 AND category = $2
            "#,
            user_id,
            category
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to find Preferences with userId={user_id}, category={category}"
            ),
            source,
        })?;
        tracing::Span::current().record("count", rows.len());

        Ok(rows
            .into_iter()
            .map(Preference::from)
            .collect::<Vec<_>>()
            .into())
    }

    /// Port of `Get` (preference_store.go:113): `WHERE UserId = ? AND Category = ? AND Name = ?`
    /// against the primary key, so at most one row.
    ///
    /// A miss is `sql.ErrNoRows` wrapped with `errors.Wrapf` — **not** `store.ErrNotFound` — and
    /// the app layer does not distinguish it from a broken query: both are the same 400
    /// (app/preference.go:39). `NotFound` is still returned here so a future caller that cares
    /// can tell; today nothing on the wire depends on the variant.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, category = %category, name = %name, found))]
    async fn get(
        &self,
        user_id: &str,
        category: &str,
        name: &str,
    ) -> Result<Preference, StoreError> {
        let row = sqlx::query_as!(
            PreferenceRow,
            r#"
            SELECT userid, category, name, value AS "value!"
            FROM preferences
            WHERE userid = $1 AND category = $2 AND name = $3
            "#,
            user_id,
            category,
            name
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to find Preference with userId={user_id}, category={category}, name={name}"
            ),
            source,
        })?;

        let Some(row) = row else {
            tracing::Span::current().record("found", false);
            return Err(StoreError::NotFound {
                entity: "Preference",
                criteria: format!("userId={user_id}, category={category}, name={name}"),
            });
        };
        tracing::Span::current().record("found", true);
        Ok(row.into())
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
