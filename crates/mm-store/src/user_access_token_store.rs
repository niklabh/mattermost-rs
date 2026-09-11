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

    /// Port of `SqlUserAccessTokenStore.Save` (line 43).
    ///
    /// Go calls `PreSave()` then `IsValid()` **inside the store**, so the id and `IsActive` are
    /// assigned here and a validation failure surfaces as a store error the app layer passes
    /// through unchanged (`errors.As(nErr, &appErr)`) — which is why an over-long description is
    /// a **400** from a route whose other failures are 500s. The token is taken by value and
    /// returned, because `PreSave` rewrites the caller's id.
    fn save(
        &self,
        token: UserAccessToken,
    ) -> impl std::future::Future<Output = Result<UserAccessToken, StoreError>> + Send;

    /// Port of `SqlUserAccessTokenStore.Delete` (line 63) — the token row **and the session it
    /// minted**, in one transaction.
    fn delete(
        &self,
        token_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlUserAccessTokenStore.Search` (line 193).
    ///
    /// **Not a substring search.** The term is passed through `sanitizeSearchTerm` — which
    /// escapes `%` and `_` — and then bound into three `LIKE`s with **no `%` of its own**, so
    /// every wildcard a caller writes becomes a literal and the match is an equality. Searching
    /// `seed` finds nothing when the username is `seed-bot`; searching `%` finds nothing at all.
    /// Measured against the running Go server, because a port that "fixed" this into
    /// `%…%` would return rows Go does not.
    fn search(
        &self,
        term: &str,
    ) -> impl std::future::Future<Output = Result<Vec<UserAccessToken>, StoreError>> + Send;

    /// Port of `SqlUserAccessTokenStore.UpdateTokenEnable` (line 212).
    ///
    /// One statement and **no transaction** — the asymmetry with
    /// [`UserAccessTokenStore::update_token_disable`] is Go's, and it is correct: re-enabling a
    /// token has no session to clean up, because disabling it deleted them.
    fn update_token_enable(
        &self,
        token_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlUserAccessTokenStore.UpdateTokenDisable` (line 219).
    ///
    /// Deletes the sessions *then* flips `IsActive`, in one transaction. The order is forced: the
    /// join reads `UserAccessTokens.Token`, not `IsActive`, so it would still work reversed — but
    /// a crash between the two statements must not leave a live session on a disabled token.
    fn update_token_disable(
        &self,
        token_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlUserAccessTokenStore.UpdateTokenRotate` (line 497).
    ///
    /// **The `DELETE` must precede the `UPDATE`.** The session join matches on the token's *old*
    /// secret (`o.Token = s.Token`), so rotating first would orphan every session minted by the
    /// previous secret — still valid, still authenticating, and no longer reachable from the row
    /// that would have revoked them. Go says so in its own comment; it is the one ordering in
    /// this file that is a security property rather than a style.
    fn update_token_rotate(
        &self,
        token_id: &str,
        new_token: &str,
        expires_at: i64,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlUserAccessTokenStore.DeleteNonCompliantExpiry` (line 383) — one bounded batch.
    ///
    /// Returns the **user id per deleted token**, not a count and not a set: the caller counts the
    /// rows to build its total and de-duplicates the ids itself. A `limit <= 0` returns an empty
    /// list without touching the database, which is also Go's guard against the `int -> uint64`
    /// cast wrapping a negative bound into an enormous one.
    fn delete_non_compliant_expiry(
        &self,
        max_expires_at: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<String>, StoreError>> + Send;
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

    #[tracing::instrument(skip_all, fields(user_id = %token.user_id, token_id))]
    async fn save(&self, mut token: UserAccessToken) -> Result<UserAccessToken, StoreError> {
        // `token.PreSave()` then `token.IsValid()`, in the store and in that order — Go's, and it
        // is why the id the caller passed is discarded rather than validated.
        token.pre_save();
        if let Err(app_error) = token.is_valid() {
            return Err(StoreError::Invalid {
                entity: "UserAccessToken",
                app_error,
            });
        }
        tracing::Span::current().record("token_id", &token.id);

        // Six columns. `LastNotifiedAt` is **not** among them, so a fresh row takes the column's
        // NULL default — which is the "never warned" marker the expiry job reads.
        sqlx::query!(
            r#"
            INSERT INTO useraccesstokens (id, token, userid, description, isactive, expiresat)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
            token.id,
            token.token,
            token.user_id,
            token.description,
            token.is_active,
            token.expires_at
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to save UserAccessToken".to_owned(),
            source,
        })?;

        Ok(token)
    }

    #[tracing::instrument(skip_all, fields(token_id = %token_id))]
    async fn delete(&self, token_id: &str) -> Result<(), StoreError> {
        let mut transaction = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "begin_transaction".to_owned(),
            source,
        })?;

        // The session join reads the **secret**, not the id: a session minted by this token holds
        // the same string in `Sessions.Token`. So the two rows are linked only through a value
        // that is about to be deleted, which is why both statements live in one transaction.
        let deleted = delete_sessions_for_token(&mut transaction, token_id).await;
        let deleted = match deleted {
            Ok(()) => sqlx::query!("DELETE FROM useraccesstokens WHERE id = $1", token_id)
                .execute(&mut *transaction)
                .await
                .map(|_| ())
                .map_err(|source| StoreError::Db {
                    context: format!("failed to delete UserAccessToken id={token_id}"),
                    source,
                }),
            Err(err) => Err(err),
        };

        match deleted {
            Ok(()) => transaction.commit().await.map_err(|source| StoreError::Db {
                context: "commit_transaction".to_owned(),
                source,
            }),
            // **Go returns `nil` here, and that is not a typo on this side.** `Delete` guards the
            // commit with `if err := …; err == nil` and then falls through to `return nil`, so a
            // failed delete is rolled back and reported to the caller as success — the route
            // answers `{"status":"OK"}` having deleted nothing. Reproduced because it is on the
            // wire; logged at error level because nothing else would record it.
            Err(err) => {
                tracing::error!(error = %err, token_id, "deleting a UserAccessToken failed; Go reports success anyway");
                Ok(())
            }
        }
    }

    #[tracing::instrument(skip_all, fields(found))]
    async fn search(&self, term: &str) -> Result<Vec<UserAccessToken>, StoreError> {
        // `sanitizeSearchTerm(term, "\\")` first — it strips every backslash, then escapes `%`
        // and `_` with one. Postgres's default `LIKE` escape is the backslash, so Go needs no
        // `ESCAPE` clause and neither does this. The net effect is an equality: no `%` is added
        // around the term and any the caller wrote is now literal.
        //
        // The `INNER JOIN Users` is a filter as well as a source for `Username`: a token whose
        // owner row is gone cannot match, however well its own id matches.
        let term = crate::user_store::sanitize_search_term(term, '\\');
        let rows = sqlx::query_as!(
            TokenRow,
            r#"
            SELECT uat.id                          AS "id!",
                   COALESCE(uat.token, '')         AS "token!",
                   COALESCE(uat.userid, '')        AS "userid!",
                   COALESCE(uat.description, '')   AS "description!",
                   COALESCE(uat.isactive, false)   AS "isactive!",
                   uat.expiresat                   AS "expiresat!",
                   uat.lastnotifiedat              AS "lastnotifiedat?"
              FROM useraccesstokens uat
             INNER JOIN users u ON uat.userid = u.id
             WHERE uat.id LIKE $1 OR uat.userid LIKE $1 OR u.username LIKE $1
            "#,
            term
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            // Go interpolates the **sanitised** term here, not the caller's; same here.
            context: format!("failed to find UserAccessTokens by term with value '{term}'"),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        Ok(rows.into_iter().map(UserAccessToken::from).collect())
    }

    #[tracing::instrument(skip_all, fields(token_id = %token_id))]
    async fn update_token_enable(&self, token_id: &str) -> Result<(), StoreError> {
        sqlx::query!(
            "UPDATE useraccesstokens SET isactive = TRUE WHERE id = $1",
            token_id
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update UserAccessTokens with id={token_id}"),
            source,
        })?;

        Ok(())
    }

    #[tracing::instrument(skip_all, fields(token_id = %token_id))]
    async fn update_token_disable(&self, token_id: &str) -> Result<(), StoreError> {
        let mut transaction = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "begin_transaction".to_owned(),
            source,
        })?;

        delete_sessions_for_token(&mut transaction, token_id).await?;

        sqlx::query!(
            "UPDATE useraccesstokens SET isactive = FALSE WHERE id = $1",
            token_id
        )
        .execute(&mut *transaction)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update UserAccessToken with id={token_id}"),
            source,
        })?;

        // Unlike `delete`, this one **propagates**: Go's `UpdateTokenDisable` returns the inner
        // error rather than swallowing it, so a disable that fails is a 500 while a revoke that
        // fails is a 200. Same file, two conventions.
        transaction.commit().await.map_err(|source| StoreError::Db {
            context: "commit_transaction".to_owned(),
            source,
        })
    }

    #[tracing::instrument(skip_all, fields(token_id = %token_id, expires_at = expires_at))]
    async fn update_token_rotate(
        &self,
        token_id: &str,
        new_token: &str,
        expires_at: i64,
    ) -> Result<(), StoreError> {
        let mut transaction = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "begin_transaction".to_owned(),
            source,
        })?;

        // Sessions first. See the trait doc: the join matches the **old** secret, so an UPDATE
        // that ran first would leave every session minted by that secret alive and unreachable.
        delete_sessions_for_token(&mut transaction, token_id).await?;

        sqlx::query!(
            "UPDATE useraccesstokens SET token = $1, expiresat = $2 WHERE id = $3",
            new_token,
            expires_at,
            token_id
        )
        .execute(&mut *transaction)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to rotate UserAccessToken id={token_id}"),
            source,
        })?;

        transaction.commit().await.map_err(|source| StoreError::Db {
            context: "commit_transaction".to_owned(),
            source,
        })
    }

    #[tracing::instrument(skip_all, fields(max_expires_at, limit, deleted))]
    async fn delete_non_compliant_expiry(
        &self,
        max_expires_at: i64,
        limit: i64,
    ) -> Result<Vec<String>, StoreError> {
        if limit <= 0 {
            return Ok(Vec::new());
        }

        // One statement, three data-modifying CTEs. `deleted_sessions` has no `RETURNING` and is
        // referenced by nothing, which in Postgres still executes it — a data-modifying CTE always
        // runs. `to_delete` is evaluated once and both DELETEs read that same snapshot, which is
        // what makes the session sweep and the token sweep agree on a concurrent write.
        //
        // The three predicates are the same ones `count_non_compliant_expiry` uses, and they have
        // to stay the same ones: the count route exists to preview exactly this delete.
        let user_ids = sqlx::query_scalar!(
            r#"
            WITH to_delete AS (
                SELECT id, token, userid
                  FROM useraccesstokens
                 WHERE (expiresat = 0 OR expiresat > $1)
                   AND isactive = true
                   AND userid NOT IN (SELECT userid FROM bots)
                 LIMIT $2
            ),
            deleted_sessions AS (
                DELETE FROM sessions
                 WHERE token IN (SELECT token FROM to_delete)
            ),
            deleted_tokens AS (
                DELETE FROM useraccesstokens
                 WHERE id IN (SELECT id FROM to_delete)
             RETURNING userid
            )
            SELECT userid AS "userid!" FROM deleted_tokens
            "#,
            max_expires_at,
            limit
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to delete non-compliant UserAccessTokens".to_owned(),
            source,
        })?;

        tracing::Span::current().record("deleted", user_ids.len());
        Ok(user_ids)
    }
}

/// The one statement three write paths share: `DELETE FROM Sessions s USING UserAccessTokens o
/// WHERE o.Token = s.Token AND o.Id = ?` (user_access_token_store.go:82, :476, :504).
///
/// Written out once because the three callers must not drift: revoke, disable and rotate all
/// invalidate the sessions the token minted, and a copy that lost the `o.Id` predicate would
/// delete every session on the installation.
async fn delete_sessions_for_token(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    token_id: &str,
) -> Result<(), StoreError> {
    sqlx::query!(
        r#"
        DELETE FROM sessions s
              USING useraccesstokens o
              WHERE o.token = s.token
                AND o.id = $1
        "#,
        token_id
    )
    .execute(&mut **transaction)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to delete Sessions with UserAccessToken id={token_id}"),
        source,
    })?;

    Ok(())
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

    /// A store pointing at nothing, so a method that is supposed to answer **without querying**
    /// can be shown to do so: any statement at all would fail here.
    ///
    /// `acquire_timeout` is capped because sqlx's default is 30 seconds and a test that waits is
    /// a bug; `connect_lazy` defers the connection attempt to first use, which is the point.
    fn store_that_cannot_query() -> SqlUserAccessTokenStore {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nothing")
            .expect("a lazy pool needs no server");
        SqlUserAccessTokenStore::new(pool)
    }

    /// Go's `DeleteNonCompliantExpiry` returns early on `limit <= 0`, and its comment says why:
    /// the SQL takes `uint64(limit)`, so a negative bound would wrap into an enormous one and
    /// delete the whole table instead of a bounded batch. The guard is the only thing between a
    /// caller's arithmetic slip and every personal access token on the installation.
    #[tokio::test]
    async fn a_non_positive_batch_limit_deletes_nothing_and_queries_nothing() {
        let store = store_that_cannot_query();

        for limit in [0, -1, -1000] {
            let deleted = store
                .delete_non_compliant_expiry(1_788_600_000_000, limit)
                .await
                .expect("the guard answers before the pool is touched");
            assert!(deleted.is_empty(), "limit {limit}");
        }
    }
}
