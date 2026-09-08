//! Port of `SqlUserAccessTokenStore` (channels/store/sqlstore/user_access_token_store.go) — the
//! reads behind the four personal-access-token routes.
//!
//! # Neither list query has an `ORDER BY`
//!
//! `GetAll` and `GetByUser` are `SELECT … LIMIT … OFFSET …` and nothing else (lines 149, 178), so
//! their order is whatever Postgres returns and their *pages* are only stable while the table is.
//! Reproduced verbatim: adding an `ORDER BY Id` here would make this server's paging more defined
//! than Go's, which turns an unspecified ordering into a divergence a parity test then has to
//! encode. The suite compares such a page as a set.
//!
//! # `Token` is selected and must be cleared by the caller
//!
//! The projection includes the secret (line 31) because `GetByToken` authenticates with it. Every
//! **route** blanks it in the app layer (`token.Token = ""`), which is where the sanitisation
//! lives in Go too — so a new caller of this store gets the secret unless it asks not to. See
//! `mm_app::user_access_token`.

use mm_model::user_access_token::UserAccessToken;
use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `store.UserAccessTokenStore` the four read routes need.
pub trait UserAccessTokenStore {
    /// Port of `SqlUserAccessTokenStore.Get` (line 134). `ErrNotFound` on a miss.
    fn get(
        &self,
        token_id: &str,
    ) -> impl std::future::Future<Output = Result<UserAccessToken, StoreError>> + Send;

    /// Port of `SqlUserAccessTokenStore.GetAll` (line 149). Go takes an **offset**, not a page.
    fn get_all(
        &self,
        offset: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<UserAccessToken>, StoreError>> + Send;

    /// Port of `SqlUserAccessTokenStore.GetByUser` (line 178).
    fn get_by_user(
        &self,
        user_id: &str,
        offset: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<UserAccessToken>, StoreError>> + Send;

    /// Port of `SqlUserAccessTokenStore.CountNonCompliantExpiry` (line 360).
    ///
    /// Three predicates, and each one narrows for a different reason: a token is non-compliant if
    /// it **never expires** (`ExpiresAt = 0`) *or* expires beyond the policy window, **and** it is
    /// active, **and** its owner is not a bot. Dropping any one of them changes the number an
    /// administrator is shown before a revocation sweep.
    fn count_non_compliant_expiry(
        &self,
        max_expires_at: i64,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlUserAccessTokenStore {
    pool: PgPool,
}

impl SqlUserAccessTokenStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// One row of `userAccessTokensSelectQuery` (user_access_token_store.go:28-38).
///
/// `ExpiresAt` is the one column with a `NOT NULL DEFAULT 0`; the rest are nullable while Go scans
/// them into plain fields, so the `COALESCE`s give the zero value Go's struct would hold.
/// `LastNotifiedAt` stays an `Option` because the model's is one — it is `json:"-"` and never
/// reaches a client either way.
struct TokenRow {
    id: String,
    token: String,
    userid: String,
    description: String,
    isactive: bool,
    expiresat: i64,
    lastnotifiedat: Option<i64>,
}

impl From<TokenRow> for UserAccessToken {
    fn from(row: TokenRow) -> Self {
        UserAccessToken {
            id: row.id,
            token: row.token,
            user_id: row.userid,
            description: row.description,
            is_active: row.isactive,
            expires_at: row.expiresat,
            last_notified_at: row.lastnotifiedat,
        }
    }
}

impl UserAccessTokenStore for SqlUserAccessTokenStore {
    #[tracing::instrument(skip_all, fields(token_id = %token_id))]
    async fn get(&self, token_id: &str) -> Result<UserAccessToken, StoreError> {
        let row = sqlx::query_as!(
            TokenRow,
            r#"
            SELECT id                          AS "id!",
                   COALESCE(token, '')         AS "token!",
                   COALESCE(userid, '')        AS "userid!",
                   COALESCE(description, '')   AS "description!",
                   COALESCE(isactive, false)   AS "isactive!",
                   expiresat                   AS "expiresat!",
                   lastnotifiedat              AS "lastnotifiedat?"
              FROM useraccesstokens
             WHERE id = $1
            "#,
            token_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get UserAccessToken with id={token_id}"),
            source,
        })?;

        row.map(UserAccessToken::from)
            .ok_or_else(|| StoreError::NotFound {
                entity: "UserAccessToken",
                criteria: format!("id={token_id}"),
            })
    }

    #[tracing::instrument(skip_all, fields(offset, limit, found))]
    async fn get_all(&self, offset: i64, limit: i64) -> Result<Vec<UserAccessToken>, StoreError> {
        let rows = sqlx::query_as!(
            TokenRow,
            r#"
            SELECT id                          AS "id!",
                   COALESCE(token, '')         AS "token!",
                   COALESCE(userid, '')        AS "userid!",
                   COALESCE(description, '')   AS "description!",
                   COALESCE(isactive, false)   AS "isactive!",
                   expiresat                   AS "expiresat!",
                   lastnotifiedat              AS "lastnotifiedat?"
              FROM useraccesstokens
             LIMIT $1 OFFSET $2
            "#,
            limit,
            offset
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find UserAccessTokens".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        Ok(rows.into_iter().map(UserAccessToken::from).collect())
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, offset, limit, found))]
    async fn get_by_user(
        &self,
        user_id: &str,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<UserAccessToken>, StoreError> {
        let rows = sqlx::query_as!(
            TokenRow,
            r#"
            SELECT id                          AS "id!",
                   COALESCE(token, '')         AS "token!",
                   COALESCE(userid, '')        AS "userid!",
                   COALESCE(description, '')   AS "description!",
                   COALESCE(isactive, false)   AS "isactive!",
                   expiresat                   AS "expiresat!",
                   lastnotifiedat              AS "lastnotifiedat?"
              FROM useraccesstokens
             WHERE userid = $1
             LIMIT $2 OFFSET $3
            "#,
            user_id,
            limit,
            offset
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find UserAccessTokens with userId={user_id}"),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        Ok(rows.into_iter().map(UserAccessToken::from).collect())
    }

    #[tracing::instrument(skip_all, fields(max_expires_at, count))]
    async fn count_non_compliant_expiry(&self, max_expires_at: i64) -> Result<i64, StoreError> {
        // `UserId NOT IN (SELECT UserId FROM Bots)` is Go's own subquery, spelled the same way.
        // `NOT IN` against a column that can be NULL yields no rows at all if any NULL is present;
        // `Bots.UserId` is `NOT NULL`, so the shape is safe here and would not be on another table.
        let count = sqlx::query_scalar!(
            r#"
            SELECT COUNT(*) AS "count!"
              FROM useraccesstokens
             WHERE (expiresat = 0 OR expiresat > $1)
               AND isactive = true
               AND userid NOT IN (SELECT userid FROM bots)
            "#,
            max_expires_at
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to count non-compliant UserAccessTokens".to_owned(),
            source,
        })?;

        tracing::Span::current().record("count", count);
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The row carries the **secret**, and the mapping keeps it. Sanitisation is the app layer's
    /// job in Go and here; a store that blanked it would break `GetByToken` authentication.
    #[test]
    fn a_row_keeps_the_token_for_the_app_layer_to_clear() {
        let token: UserAccessToken = TokenRow {
            id: "j1x3z8ynqjbstd4c4k6qy1p7ph".to_owned(),
            token: "cqjc7ec6bpy65jjamstkhpe6fr".to_owned(),
            userid: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            description: "a personal access token".to_owned(),
            isactive: true,
            expiresat: 0,
            lastnotifiedat: None,
        }
        .into();

        assert_eq!(token.token, "cqjc7ec6bpy65jjamstkhpe6fr");
        assert!(token.is_active);
        assert_eq!(token.expires_at, 0, "zero means never expires");
        assert_eq!(token.last_notified_at, None);
    }
}
