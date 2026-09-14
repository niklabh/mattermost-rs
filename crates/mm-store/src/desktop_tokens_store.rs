//! Port of `SqlDesktopTokensStore` (channels/store/sqlstore/desktop_tokens_store.go), narrowed
//! to the three methods `ValidateDesktopToken` reaches from `POST /api/v4/users/login/desktop_token`.
//!
//! `DesktopTokens` is the hand-off table between an SSO login completed in a browser and the
//! desktop app that started it: the browser-side completion writes a 64-character token with
//! the user id and a `CreateAt`, the desktop app posts the token back, and the login here trades
//! it for a session. The table has three columns, the token is the primary key, and there is
//! **no foreign key** to `Users`.
//!
//! # `CreateAt` is Unix **seconds**
//!
//! Both writers pass `time.Now().Unix()` (web/oauth.go:390, web/saml.go:220) and the reader
//! compares against `time.Now().Add(-DesktopTokenTTL).Unix()` (api4/user.go:2310). The second
//! non-millisecond timestamp in the migrated surface, after `PostReminders.TargetTime`.
//!
//! `Insert` and `DeleteOlderThan` are not ported: the first is written only by the SSO completion
//! pages, which are not served here, and the second by the expiry job.

use sqlx::PgPool;

use crate::error::StoreError;

/// Port of `store.DesktopTokensStore`, narrowed to what the login reaches.
pub trait DesktopTokensStore {
    /// Port of `SqlDesktopTokensStore.GetUserId` (desktop_tokens_store.go:24): the user id
    /// behind `token`, provided the row's `CreateAt` is **at or after** `min_create_at`.
    ///
    /// The expiry is the reader's, not the row's: a token is "expired" when it is older than
    /// the caller's cut-off, and that is answered with the same [`StoreError::NotFound`] as a
    /// token that was never written. The app layer cannot tell the two apart and does not try.
    fn get_user_id(
        &self,
        token: &str,
        min_create_at: i64,
    ) -> impl std::future::Future<Output = Result<String, StoreError>> + Send;

    /// Port of `SqlDesktopTokensStore.Delete` (desktop_tokens_store.go:63): the row for `token`,
    /// if any. Deleting an absent row is not an error.
    fn delete(
        &self,
        token: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlDesktopTokensStore.DeleteByUserId` (desktop_tokens_store.go:80): every row
    /// for `user_id`, whatever its age. Also not an error when there is nothing to delete.
    fn delete_by_user_id(
        &self,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;
}

#[derive(Debug, Clone)]
pub struct SqlDesktopTokensStore {
    pool: PgPool,
}

impl SqlDesktopTokensStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl DesktopTokensStore for SqlDesktopTokensStore {
    #[tracing::instrument(skip(self, token), fields(min_create_at))]
    async fn get_user_id(&self, token: &str, min_create_at: i64) -> Result<String, StoreError> {
        // `sq.GtOrEq{"CreateAt": minCreateAt}` — a token written in the very second of the
        // cut-off is still good.
        let row = sqlx::query_scalar!(
            r#"
            SELECT userid AS "user_id!"
              FROM desktoptokens
             WHERE token = $1 AND createat >= $2
            "#,
            token,
            min_create_at,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("No token for {token}"),
            source,
        })?;

        row.ok_or_else(|| StoreError::NotFound {
            entity: "DesktopTokens",
            criteria: token.to_owned(),
        })
    }

    #[tracing::instrument(skip(self, token))]
    async fn delete(&self, token: &str) -> Result<(), StoreError> {
        sqlx::query!("DELETE FROM desktoptokens WHERE token = $1", token)
            .execute(&self.pool)
            .await
            .map_err(|source| StoreError::Db {
                context: "failed to delete token row".to_owned(),
                source,
            })?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(user_id = %user_id))]
    async fn delete_by_user_id(&self, user_id: &str) -> Result<(), StoreError> {
        sqlx::query!("DELETE FROM desktoptokens WHERE userid = $1", user_id)
            .execute(&self.pool)
            .await
            .map_err(|source| StoreError::Db {
                context: "failed to delete token row".to_owned(),
                source,
            })?;
        Ok(())
    }
}
