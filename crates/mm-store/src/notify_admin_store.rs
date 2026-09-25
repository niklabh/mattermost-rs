//! Port of `SqlNotifyAdminStore` (channels/store/sqlstore/notify_admin_store.go): `Save` and
//! `GetDataByUserIdAndFeature`, the two reads and writes behind `POST /api/v4/users/notify-admin`.
//!
//! `NotifyAdmin` is the table of "a user asked an admin to upgrade" rows: one per
//! `(UserId, RequiredFeature, RequiredPlan)`, which is the primary key. `SentAt` is filled by the
//! notification job when it mails the admins; nothing on the migrated surface writes it, and it
//! reads back as Go's `sql.NullInt64` — `{"Int64":0,"Valid":false}` for the NULL every new row
//! carries.

use mm_model::notify_admin::{MattermostFeature, NotifyAdminData, NullInt64};
use sqlx::PgPool;

use crate::error::StoreError;

/// Port of `store.NotifyAdminStore`.
pub trait NotifyAdminStore {
    /// Port of `SqlNotifyAdminStore.Save` (notify_admin_store.go:48): `IsValid`, then `PreSave`
    /// (the `CreateAt` stamp), then one `INSERT` of five columns — `SentAt` is not among them.
    ///
    /// The validator's refusal is returned as [`StoreError::Invalid`] and the app layer turns it
    /// into a **500**, not the 400 the validator built: `SaveAdminNotifyData` tests the error for
    /// `*store.ErrNotFound` and folds everything else into `app.notify_admin.save.app_error`. So
    /// a bad plan or feature is a 500 on the wire, and the validator's message is lost.
    ///
    /// A second row for the same key is a primary-key violation, also a 500 — reachable only by
    /// a second request with the same feature *and* a different plan slipping past
    /// `UserAlreadyNotifiedOnRequiredFeature`, which it cannot, since that check is by feature.
    fn save(
        &self,
        data: &NotifyAdminData,
    ) -> impl std::future::Future<Output = Result<NotifyAdminData, StoreError>> + Send;

    /// Port of `SqlNotifyAdminStore.GetDataByUserIdAndFeature` (notify_admin_store.go:63): every
    /// row for the user and feature, any plan, sent or not.
    ///
    /// Go's `sql.ErrNoRows` arm is dead — `Select` into a slice never returns it — so a user
    /// with no rows is an empty list, which is what `UserAlreadyNotifiedOnRequiredFeature`
    /// reads as "not yet notified". No `ORDER BY`.
    fn get_data_by_user_id_and_feature(
        &self,
        user_id: &str,
        feature: &MattermostFeature,
    ) -> impl std::future::Future<Output = Result<Vec<NotifyAdminData>, StoreError>> + Send;

    /// Port of `SqlNotifyAdminStore.Get` (notify_admin_store.go:82): the rows of this `trial`
    /// flavour not yet sent (`SentAt IS NULL`). No `ORDER BY`.
    fn get(
        &self,
        trial: bool,
    ) -> impl std::future::Future<Output = Result<Vec<NotifyAdminData>, StoreError>> + Send;

    /// Port of `SqlNotifyAdminStore.Update` (notify_admin_store.go:105): `SentAt = now` for the
    /// one key. Whatever `Trial` is.
    fn update(
        &self,
        user_id: &str,
        required_plan: &str,
        required_feature: &MattermostFeature,
        now: i64,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlNotifyAdminStore.DeleteBefore` (notify_admin_store.go:98): the unsent rows of
    /// this `trial` flavour created **strictly** before `now`.
    fn delete_before(
        &self,
        trial: bool,
        now: i64,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;
}

/// A `NotifyAdmin` row as `notifyAdminQuery` (`SELECT *`) reads it.
fn notify_admin_from(
    user_id: String,
    create_at: Option<i64>,
    required_plan: String,
    required_feature: String,
    trial: bool,
    sent_at: Option<i64>,
) -> NotifyAdminData {
    NotifyAdminData {
        create_at: create_at.unwrap_or_default(),
        user_id,
        required_plan,
        required_feature: MattermostFeature(required_feature),
        trial,
        sent_at: match sent_at {
            Some(int64) => NullInt64 { int64, valid: true },
            None => NullInt64::default(),
        },
    }
}

#[derive(Debug, Clone)]
pub struct SqlNotifyAdminStore {
    pool: PgPool,
}

impl SqlNotifyAdminStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl NotifyAdminStore for SqlNotifyAdminStore {
    #[tracing::instrument(skip(self, data), fields(user_id = %data.user_id, feature = %data.required_feature))]
    async fn save(&self, data: &NotifyAdminData) -> Result<NotifyAdminData, StoreError> {
        data.is_valid().map_err(|app_error| StoreError::Invalid {
            entity: "NotifyAdmin",
            app_error,
        })?;

        // Owned because `PreSave` stamps the row Go was handed, and the caller gets it back.
        let mut data = data.clone();
        data.pre_save();

        sqlx::query!(
            r#"
            INSERT INTO notifyadmin (userid, createat, requiredplan, requiredfeature, trial)
            VALUES ($1, $2, $3, $4, $5)
            "#,
            data.user_id,
            data.create_at,
            data.required_plan,
            data.required_feature.as_str(),
            data.trial,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to save Notify Admin data".to_owned(),
            source,
        })?;

        Ok(data)
    }

    #[tracing::instrument(skip(self), fields(user_id = %user_id, feature = %feature, found))]
    async fn get_data_by_user_id_and_feature(
        &self,
        user_id: &str,
        feature: &MattermostFeature,
    ) -> Result<Vec<NotifyAdminData>, StoreError> {
        let rows = sqlx::query!(
            r#"
            SELECT userid                 AS "user_id!",
                   createat               AS "create_at?",
                   requiredplan           AS "required_plan!",
                   requiredfeature        AS "required_feature!",
                   trial                  AS "trial!",
                   sentat                 AS "sent_at?"
              FROM notifyadmin
             WHERE userid = $1 AND requiredfeature = $2
            "#,
            user_id,
            feature.as_str(),
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "notifcation data by user id: {user_id} and required feature: {feature}"
            ),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        Ok(rows
            .into_iter()
            .map(|row| {
                notify_admin_from(
                    row.user_id,
                    row.create_at,
                    row.required_plan,
                    row.required_feature,
                    row.trial,
                    row.sent_at,
                )
            })
            .collect())
    }

    #[tracing::instrument(skip(self), fields(trial, found))]
    async fn get(&self, trial: bool) -> Result<Vec<NotifyAdminData>, StoreError> {
        let rows = sqlx::query!(
            r#"
            SELECT userid                 AS "user_id!",
                   createat               AS "create_at?",
                   requiredplan           AS "required_plan!",
                   requiredfeature        AS "required_feature!",
                   trial                  AS "trial!",
                   sentat                 AS "sent_at?"
              FROM notifyadmin
             WHERE trial = $1 AND sentat IS NULL
            "#,
            trial,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "notifcation data".to_owned(),
            source,
        })?;
        tracing::Span::current().record("found", rows.len());
        Ok(rows
            .into_iter()
            .map(|row| {
                notify_admin_from(
                    row.user_id,
                    row.create_at,
                    row.required_plan,
                    row.required_feature,
                    row.trial,
                    row.sent_at,
                )
            })
            .collect())
    }

    #[tracing::instrument(skip(self), fields(user_id = %user_id, plan = %required_plan))]
    async fn update(
        &self,
        user_id: &str,
        required_plan: &str,
        required_feature: &MattermostFeature,
        now: i64,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            "UPDATE notifyadmin SET sentat = $1 \
             WHERE userid = $2 AND requiredplan = $3 AND requiredfeature = $4",
            now,
            user_id,
            required_plan,
            required_feature.as_str(),
        )
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to update SentAt for userId={user_id} and requiredPlan={required_plan}"
            ),
            source,
        })
    }

    #[tracing::instrument(skip(self), fields(trial, now))]
    async fn delete_before(&self, trial: bool, now: i64) -> Result<(), StoreError> {
        sqlx::query!(
            "DELETE FROM notifyadmin WHERE trial = $1 AND createat < $2 AND sentat IS NULL",
            trial,
            now,
        )
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|source| StoreError::Db {
            context: format!("failed to remove all notification data with trial={trial}"),
            source,
        })
    }
}
