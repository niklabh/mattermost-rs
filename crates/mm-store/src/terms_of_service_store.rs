//! Port of `SqlTermsOfServiceStore` (channels/store/sqlstore/terms_of_service_store.go):
//! `GetLatest`, `Get` and `Save`.
//!
//! `GetLatest` serves `getLatestTermsOfService` (api4/terms_of_service.go:20); `Get` is what
//! `saveUserTermsOfService` (api4/user.go:3471) checks the posted id against; `Save` is
//! `createTermsOfService`, which is licence-gated on `CustomTermsOfService` and so refuses before
//! it reaches this on an unlicensed installation.

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

    /// Port of `SqlTermsOfServiceStore.Get` (terms_of_service_store.go:76).
    ///
    /// A miss is `ErrNotFound("TermsOfService", "id")` — the **literal string `id`**, not the id
    /// that was asked for, unlike every neighbouring store. Reproduced; it is a log line, not
    /// wire format, but a reader comparing the two servers' logs would otherwise see a difference
    /// this port invented.
    fn get(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<TermsOfService, StoreError>> + Send;

    /// Port of `SqlTermsOfServiceStore.Save` (terms_of_service_store.go:37).
    ///
    /// Three steps before the insert, in Go's order: a non-empty `Id` is refused outright with
    /// `ErrInvalidInput` (**the app layer turns that into a 400**, not a 500), then `PreSave`
    /// mints the id and stamps `CreateAt`, then `IsValid`. Taken by value and handed back,
    /// because `PreSave` is what fills the row.
    fn save(
        &self,
        terms: TermsOfService,
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

    #[tracing::instrument(skip(self), fields(terms_id = %id, found))]
    async fn get(&self, id: &str) -> Result<TermsOfService, StoreError> {
        let row = sqlx::query_as!(
            TermsOfServiceRow,
            r#"
            SELECT id                    AS "id!",
                   COALESCE(createat, 0) AS "createat!",
                   COALESCE(userid, '')  AS "userid!",
                   COALESCE(text, '')    AS "text!"
              FROM termsofservice
             WHERE id = $1
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("could not find TermsOfService with id={id}"),
            source,
        })?
        .ok_or(StoreError::NotFound {
            entity: "TermsOfService",
            // Go's criteria is the literal `"id"` here — see the trait's doc comment.
            criteria: String::from("id"),
        })?;

        tracing::Span::current().record("found", true);
        Ok(TermsOfService {
            id: row.id,
            create_at: row.createat,
            user_id: row.userid,
            text: row.text,
        })
    }

    /// # The id guard runs before `PreSave`, which is what makes it reachable at all
    ///
    /// `PreSave` mints an id when one is missing, so if the check came after it no caller could
    /// ever trip it. Coming first, it refuses a client that supplied its own id — the one thing
    /// that would let a caller overwrite a published revision.
    #[tracing::instrument(skip_all, fields(terms_id))]
    async fn save(&self, mut terms: TermsOfService) -> Result<TermsOfService, StoreError> {
        if !terms.id.is_empty() {
            return Err(StoreError::InvalidInput {
                entity: "TermsOfService",
                field: "Id",
                value: terms.id,
            });
        }

        terms.pre_save();
        if let Err(app_error) = terms.is_valid() {
            return Err(StoreError::Invalid {
                entity: "TermsOfService",
                app_error,
            });
        }
        tracing::Span::current().record("terms_id", &terms.id);

        sqlx::query!(
            r#"
            INSERT INTO termsofservice (id, createat, userid, text)
            VALUES ($1, $2, $3, $4)
            "#,
            terms.id,
            terms.create_at,
            terms.user_id,
            terms.text,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "could not save a new TermsOfService".to_owned(),
            source,
        })?;

        Ok(terms)
    }
}

/// One row of `termsOfServiceSelectQuery` (terms_of_service_store.go:30-32).
struct TermsOfServiceRow {
    id: String,
    createat: i64,
    userid: String,
    text: String,
}
