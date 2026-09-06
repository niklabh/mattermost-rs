//! Port of `SqlTermsOfServiceStore` (channels/store/sqlstore/terms_of_service_store.go),
//! `GetLatest` only.
//!
//! Ported for `getLatestTermsOfService` (api4/terms_of_service.go:20). `Get(id)` and `Save` are
//! not: no migrated route reads a revision by id, and publishing one is licence-gated
//! (`CustomTermsOfService`) and therefore unreachable on this deployment.

use mm_model::terms_of_service::TermsOfService;
use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `store.TermsOfServiceStore` (store/store.go) that is ported.
pub trait TermsOfServiceStore {
    /// Port of `SqlTermsOfServiceStore.GetLatest` (terms_of_service_store.go:60).
    ///
    /// Go's `allowFromCache` parameter is not in this signature: it selects the **cache layer's**
    /// read (localcachelayer/terms_of_service_layer.go:47) and the SQL store ignores it entirely.
    /// Nothing on this side caches.
    fn get_latest(
        &self,
    ) -> impl std::future::Future<Output = Result<TermsOfService, StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlTermsOfServiceStore {
    pool: PgPool,
}

impl SqlTermsOfServiceStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl TermsOfServiceStore for SqlTermsOfServiceStore {
    /// # `ORDER BY CreateAt DESC LIMIT 1`, and no tiebreak
    ///
    /// Two revisions published in the same millisecond have no defined order, so "the latest" is
    /// whichever the heap hands back first. Reproduced without an `Id` tiebreak: adding one would
    /// make our answer *more* defined than Go's, which is a divergence that shows up as a flake
    /// somewhere else.
    ///
    /// An empty table is `ErrNotFound`, which the app layer answers **404** to — not an empty
    /// object.
    #[tracing::instrument(skip_all, fields(found))]
    async fn get_latest(&self) -> Result<TermsOfService, StoreError> {
        let row = sqlx::query_as!(
            TermsOfServiceRow,
            r#"
            SELECT id                    AS "id!",
                   COALESCE(createat, 0) AS "createat!",
                   COALESCE(userid, '')  AS "userid!",
                   COALESCE(text, '')    AS "text!"
              FROM termsofservice
             ORDER BY createat DESC
             LIMIT 1
            "#
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "could not find latest TermsOfService".to_owned(),
            source,
        })?
        .ok_or_else(|| StoreError::NotFound {
            entity: "TermsOfService",
            criteria: "CreateAt=latest".to_owned(),
        })?;

        tracing::Span::current().record("found", true);
        Ok(TermsOfService {
            id: row.id,
            create_at: row.createat,
            user_id: row.userid,
            text: row.text,
        })
    }
}

/// One row of `termsOfServiceSelectQuery` (terms_of_service_store.go:30-32).
struct TermsOfServiceRow {
    id: String,
    createat: i64,
    userid: String,
    text: String,
}
