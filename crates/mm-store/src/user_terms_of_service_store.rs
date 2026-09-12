//! Port of `SqlUserTermsOfServiceStore` (channels/store/sqlstore/user_terms_of_service.go):
//! `GetByUser`, `Save` and `Delete`.
//!
//! `GetByUser` is the read `getUser`'s terms-of-service branch makes; `Save` and `Delete` are the
//! two halves of `saveUserTermsOfService` (api4/user.go:3447), which branches on the posted
//! `accepted` flag and never touches the other one.

use mm_model::user_terms_of_service::UserTermsOfService;
use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `store.UserTermsOfServiceStore` (store/store.go) that is ported.
pub trait UserTermsOfServiceStore {
    /// Port of `SqlUserTermsOfServiceStore.GetByUser` (user_terms_of_service.go:34).
    fn get_by_user(
        &self,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<UserTermsOfService, StoreError>> + Send;

    /// Port of `SqlUserTermsOfServiceStore.Save` (user_terms_of_service.go:51) — an **upsert
    /// spelled as UPDATE-then-INSERT**, not `ON CONFLICT`.
    ///
    /// Go runs the `UPDATE ... WHERE UserId = :UserId` first and inserts only when it affected
    /// zero rows. The order is what makes re-accepting cheap and what makes the row's `CreateAt`
    /// move every time — `PreSave` stamps it unconditionally, so accepting the same revision
    /// twice rewrites the acceptance time.
    ///
    /// `PreSave` and `IsValid` run **here**, inside the store, as they do for every other
    /// `Save` in this crate.
    fn save(
        &self,
        user_terms: UserTermsOfService,
    ) -> impl std::future::Future<Output = Result<UserTermsOfService, StoreError>> + Send;

    /// Port of `SqlUserTermsOfServiceStore.Delete` (user_terms_of_service.go:86).
    ///
    /// **Not idempotent-checked**: Go never looks at the affected row count, so deleting an
    /// acceptance that was never recorded — which is what `accepted: false` does for a user who
    /// has accepted nothing — succeeds and the route answers `{"status":"OK"}`. The predicate is
    /// on *both* columns, so rejecting revision A leaves an acceptance of revision B in place.
    fn delete(
        &self,
        user_id: &str,
        terms_of_service_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlUserTermsOfServiceStore {
    pool: PgPool,
}

impl SqlUserTermsOfServiceStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl UserTermsOfServiceStore for SqlUserTermsOfServiceStore {
    #[tracing::instrument(skip_all, fields(user_id = %user_id, found))]
    async fn get_by_user(&self, user_id: &str) -> Result<UserTermsOfService, StoreError> {
        get_by_user(&self.pool, user_id).await
    }

    #[tracing::instrument(skip_all, fields(user_id, inserted))]
    async fn save(
        &self,
        mut user_terms: UserTermsOfService,
    ) -> Result<UserTermsOfService, StoreError> {
        user_terms.pre_save();
        if let Err(app_error) = user_terms.is_valid() {
            return Err(StoreError::Invalid {
                entity: "UserTermsOfService",
                app_error,
            });
        }
        tracing::Span::current().record("user_id", &user_terms.user_id);

        let updated = sqlx::query!(
            r#"
            UPDATE usertermsofservice
               SET userid = $1, termsofserviceid = $2, createat = $3
             WHERE userid = $1
            "#,
            user_terms.user_id,
            user_terms.terms_of_service_id,
            user_terms.create_at,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to update UserTermsOfService with userId={} and termsOfServiceId={}",
                user_terms.user_id, user_terms.terms_of_service_id
            ),
            source,
        })?
        .rows_affected();

        tracing::Span::current().record("inserted", updated == 0);
        if updated == 0 {
            sqlx::query!(
                r#"
                INSERT INTO usertermsofservice (userid, termsofserviceid, createat)
                VALUES ($1, $2, $3)
                "#,
                user_terms.user_id,
                user_terms.terms_of_service_id,
                user_terms.create_at,
            )
            .execute(&self.pool)
            .await
            .map_err(|source| StoreError::Db {
                context: format!(
                    "failed to save UserTermsOfService with userId={} and termsOfServiceId={}",
                    user_terms.user_id, user_terms.terms_of_service_id
                ),
                source,
            })?;
        }

        Ok(user_terms)
    }

    #[tracing::instrument(skip(self), fields(user_id = %user_id, terms_id = %terms_of_service_id))]
    async fn delete(&self, user_id: &str, terms_of_service_id: &str) -> Result<(), StoreError> {
        sqlx::query!(
            r#"
            DELETE FROM usertermsofservice
             WHERE userid = $1 AND termsofserviceid = $2
            "#,
            user_id,
            terms_of_service_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to delete UserTermsOfService with userId={user_id} and termsOfServiceId={terms_of_service_id}"
            ),
            source,
        })?;

        Ok(())
    }
}

/// Port of `SqlUserTermsOfServiceStore.GetByUser` (user_terms_of_service.go:34).
///
/// One row by primary key (`UserId` is the table's PK — a user accepts at most one terms of
/// service; re-accepting replaces the row). The two nullable columns are coalesced through
/// `unwrap_or_default` on the Rust side rather than in SQL because Go scans them into
/// non-pointer fields the same way.
#[tracing::instrument(skip(pool), fields(user_id = %user_id))]
pub async fn get_by_user(pool: &PgPool, user_id: &str) -> Result<UserTermsOfService, StoreError> {
    let row = sqlx::query!(
        r#"
        SELECT userid, termsofserviceid, createat
          FROM usertermsofservice
         WHERE userid = $1
        "#,
        user_id
    )
    .fetch_optional(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to get UserTermsOfService with userId={user_id}"),
        source,
    })?;

    let Some(row) = row else {
        tracing::Span::current().record("found", false);
        return Err(StoreError::NotFound {
            entity: "UserTermsOfService",
            criteria: format!("userId={user_id}"),
        });
    };
    tracing::Span::current().record("found", true);

    Ok(UserTermsOfService {
        user_id: row.userid,
        terms_of_service_id: row.termsofserviceid.unwrap_or_default(),
        create_at: row.createat.unwrap_or_default(),
    })
}
