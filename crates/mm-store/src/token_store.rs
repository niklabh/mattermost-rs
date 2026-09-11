//! Port of `SqlTokenStore` (channels/store/sqlstore/tokens_store.go).
//!
//! # This is not `user_access_token_store`
//!
//! Two unrelated tables spell "token". `UserAccessTokens` holds the long-lived personal access
//! tokens a client authenticates with; `Tokens` holds **one-shot** secrets minted by the server —
//! password recovery, email verification, team invitations, the CWS handshake. They share no
//! column, no lifetime rule and no consumer. This is the second.
//!
//! # The table is nullable and the model is not
//!
//! Every column but `Token` is nullable in the schema, while `model.Token` declares four
//! non-pointer fields. Go's `sqlx.Get` therefore **fails to scan** a row with a NULL `CreateAt`,
//! `Type` or `Extra` and the caller sees a 500-shaped wrap rather than a token. Nothing writes
//! such a row: [`TokenStore::save`] always binds all four. The reads below map a NULL to the zero
//! value instead of erroring, which is a divergence only reachable by hand-editing the table —
//! recorded here rather than papered over, because a reader comparing the two would otherwise
//! think Go tolerates it.

use mm_model::token::Token;
use sqlx::PgPool;

use crate::error::StoreError;

/// Port of `store.TokenStore` (store/store.go:732), narrowed to what the password-reset and
/// email-verification routes need.
///
/// `ConsumeOnce`, `Cleanup`, `GetAllTokensByType`, `RemoveAllTokensByType` and
/// `GetTokenByTypeAndEmail` are not here: the first is the magic-link/SSO path, the second is a
/// scheduled job, and the rest serve the invitation and password-reset-*send* routes, which stay
/// with Go for want of an e-mail service ([D-219]). None is reachable from a route this server
/// answers.
pub trait TokenStore {
    /// Port of `SqlTokenStore.Save` (tokens_store.go:36).
    ///
    /// **Validation happens in the store, not the app.** `token.IsValid()` runs first and its
    /// `*model.AppError` is returned *as itself* — so a malformed token is a **500** with
    /// `model.token.is_valid.size`, not a driver error. `App.CreatePasswordRecoveryToken`
    /// depends on that: it does `errors.As(err, &appErr)` and passes the AppError through
    /// untouched.
    fn save(
        &self,
        token: &Token,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlTokenStore.GetByToken` (tokens_store.go:62).
    ///
    /// A miss is `store.NewErrNotFound("Token", "Token=<value>")`. The criteria string here says
    /// only that a token was missed — Go embeds the value, and a one-shot password-reset secret
    /// in a log line is a credential.
    fn get_by_token(
        &self,
        token: &str,
    ) -> impl std::future::Future<Output = Result<Token, StoreError>> + Send;

    /// Port of `SqlTokenStore.Delete` (tokens_store.go:54).
    ///
    /// A hard delete keyed on the token string, and **the affected-row count is ignored** — Go
    /// discards the `sql.Result`. Deleting a token that is already gone is success, which is what
    /// makes the consume-on-use path idempotent under a double submit.
    fn delete(
        &self,
        token: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;
}

#[derive(Debug, Clone)]
pub struct SqlTokenStore {
    pool: PgPool,
}

impl SqlTokenStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// One row of `Tokens`, with the schema's nullability intact. See the module docs for why the
/// three nullable columns are not an error here.
struct TokenRow {
    token: String,
    createat: Option<i64>,
    r#type: Option<String>,
    extra: Option<String>,
}

impl From<TokenRow> for Token {
    fn from(row: TokenRow) -> Self {
        Token {
            token: row.token,
            create_at: row.createat.unwrap_or_default(),
            type_: row.r#type.unwrap_or_default(),
            extra: row.extra.unwrap_or_default(),
        }
    }
}

impl TokenStore for SqlTokenStore {
    #[tracing::instrument(skip_all, fields(token_type = %token.type_))]
    async fn save(&self, token: &Token) -> Result<(), StoreError> {
        // `IsValid` before the insert, exactly as Go orders it — the column is `varchar(64)` and
        // a longer token would be a driver error rather than the AppError the app layer expects.
        if let Err(app_error) = token.is_valid() {
            return Err(StoreError::Invalid {
                entity: "Token",
                app_error,
            });
        }

        sqlx::query!(
            "INSERT INTO tokens (token, createat, type, extra) VALUES ($1, $2, $3, $4)",
            token.token,
            token.create_at,
            token.type_,
            token.extra,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to save Token".to_owned(),
            source,
        })?;

        Ok(())
    }

    #[tracing::instrument(skip_all, fields(found))]
    async fn get_by_token(&self, token: &str) -> Result<Token, StoreError> {
        let row = sqlx::query_as!(
            TokenRow,
            r#"SELECT token AS "token!", createat, type, extra FROM tokens WHERE token = $1"#,
            token
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to get Token".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", row.is_some());
        row.map(Token::from).ok_or(StoreError::NotFound {
            entity: "Token",
            criteria: "Token=<redacted>".to_owned(),
        })
    }

    #[tracing::instrument(skip_all, fields(deleted))]
    async fn delete(&self, token: &str) -> Result<(), StoreError> {
        let result = sqlx::query!("DELETE FROM tokens WHERE token = $1", token)
            .execute(&self.pool)
            .await
            .map_err(|source| StoreError::Db {
                context: "failed to delete Token".to_owned(),
                source,
            })?;

        tracing::Span::current().record("deleted", result.rows_affected());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(create_at: Option<i64>, type_: Option<&str>, extra: Option<&str>) -> TokenRow {
        TokenRow {
            token: "t".repeat(64),
            createat: create_at,
            r#type: type_.map(str::to_owned),
            extra: extra.map(str::to_owned),
        }
    }

    /// The three nullable columns become zero values rather than a scan error. See the module
    /// docs: Go fails here, and nothing can write the row that would reach it.
    #[test]
    fn nulls_become_zero_values_rather_than_errors() {
        let token = Token::from(row(None, None, None));
        assert_eq!(token.create_at, 0);
        assert_eq!(token.type_, "");
        assert_eq!(token.extra, "");
    }

    #[test]
    fn a_populated_row_maps_field_for_field() {
        let token = Token::from(row(
            Some(1_700_000_000_000),
            Some(mm_model::token::TOKEN_TYPE_PASSWORD_RECOVERY),
            Some(r#"{"UserId":"abc","Email":"a@b.c"}"#),
        ));
        assert_eq!(token.create_at, 1_700_000_000_000);
        assert_eq!(token.type_, "password_recovery");
        assert_eq!(token.extra, r#"{"UserId":"abc","Email":"a@b.c"}"#);
    }

    /// The miss message must not carry the token itself — it is a live credential until it is
    /// used, and store errors reach the server log.
    #[test]
    fn a_miss_does_not_name_the_token() {
        let err = StoreError::NotFound {
            entity: "Token",
            criteria: "Token=<redacted>".to_owned(),
        };
        assert!(!err.to_string().contains("secret"));
        assert!(err.to_string().contains("Token"));
    }
}
