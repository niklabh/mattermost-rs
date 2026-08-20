//! Port of `SqlUserTermsOfServiceStore` (channels/store/sqlstore/user_terms_of_service.go),
//! `GetByUser` only — the read `getUser`'s terms-of-service branch makes.

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
