//! Port of `SqlSessionStore` (channels/store/sqlstore/session_store.go), `Get` only.
//!
//! This is the Strangler Fig's load-bearing store. Both servers read the same `Sessions` rows, so
//! a token minted by the Go server has to authenticate here without a second login — which is
//! why this method, and not something easier, is the first one ported.

use mm_model::session::Session;
use mm_model::utils::StringMap;
use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `store.SessionStore` (store/store.go:551-571) that is ported.
///
/// Native async-in-trait (RPITIT), not `async_trait` — there is no dyn-dispatch requirement here
/// and the boxing would be pure overhead.
pub trait SessionStore {
    /// Port of `SqlSessionStore.Get` (session_store.go:87).
    fn get(
        &self,
        session_id_or_token: &str,
    ) -> impl std::future::Future<Output = Result<Session, StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlSessionStore {
    pool: PgPool,
}

impl SqlSessionStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl SessionStore for SqlSessionStore {
    #[tracing::instrument(skip_all, fields(found))]
    async fn get(&self, session_id_or_token: &str) -> Result<Session, StoreError> {
        // Go builds this with squirrel as `sq.Or{sq.Eq{"Token": x}, sq.Eq{"Id": x}}` and
        // `Limit(1)`. One bind parameter covers both sides: the argument is compared against two
        // columns, not two arguments against one column each.
        //
        // `Sessions` has no NOT NULL constraint on anything but `Id`, so every other column comes
        // back as an Option. Go scans these into non-pointer struct fields, which means a NULL
        // would be a scan error there; here it defaults, which is strictly more permissive and
        // cannot produce a wrong non-empty value. See D-078.
        let row = sqlx::query!(
            r#"
            SELECT id,
                   token,
                   createat,
                   expiresat,
                   lastactivityat,
                   userid,
                   deviceid,
                   voipdeviceid,
                   roles,
                   isoauth,
                   props,
                   expirednotify
              FROM sessions
             WHERE token = $1 OR id = $1
             LIMIT 1
            "#,
            session_id_or_token
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Sessions by id or token".to_owned(),
            source,
        })?;

        let Some(row) = row else {
            tracing::Span::current().record("found", false);
            return Err(StoreError::NotFound {
                entity: "Session",
                // Deliberately not the token itself — that value is a live credential and this
                // string reaches logs. Go interpolates it; we do not. See D-079.
                criteria: "sessionIdOrToken=<redacted>".to_owned(),
            });
        };
        tracing::Span::current().record("found", true);

        let props =
            match row.props {
                Some(value) => Some(serde_json::from_value::<StringMap>(value).map_err(
                    |source| StoreError::Decode {
                        entity: "Session",
                        column: "props",
                        source,
                    },
                )?),
                None => None,
            };

        Ok(Session {
            id: row.id,
            token: row.token.unwrap_or_default(),
            create_at: row.createat.unwrap_or_default(),
            expires_at: row.expiresat.unwrap_or_default(),
            last_activity_at: row.lastactivityat.unwrap_or_default(),
            user_id: row.userid.unwrap_or_default(),
            device_id: row.deviceid.unwrap_or_default(),
            // The only NOT NULL column on `Sessions` besides `Id`: the v11 migration that added
            // it gave it a default, so sqlx types it `String` rather than `Option<String>`.
            voip_device_id: row.voipdeviceid,
            roles: row.roles.unwrap_or_default(),
            is_oauth: row.isoauth.unwrap_or_default(),
            expired_notify: row.expirednotify.unwrap_or_default(),
            props,
            // Go's Get populates this from `Team().GetTeamsForUser`, filtered to `DeleteAt == 0`.
            // That query carries the scheme-roles join, which is a store method in its own right
            // and is not ported. Left as Go's zero value. See D-077 — this is the one place the
            // slice knowingly returns less than Go does.
            team_members: None,
            local: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The store's own error surface is testable without a database, and the API edge branches on
    /// exactly this predicate to choose 401 over 500.
    #[test]
    fn not_found_is_distinguishable_from_a_driver_error() {
        let missing = StoreError::NotFound {
            entity: "Session",
            criteria: "sessionIdOrToken=<redacted>".to_owned(),
        };
        assert!(missing.is_not_found());

        let broken = StoreError::Db {
            context: "failed to find Sessions by id or token".to_owned(),
            source: sqlx::Error::RowNotFound,
        };
        assert!(!broken.is_not_found());
    }

    /// A token is a live credential. `Get`'s miss path is the one that runs on every bad request,
    /// so it is the one most likely to end up in a log aggregator.
    #[test]
    fn not_found_message_does_not_leak_the_token() {
        let err = StoreError::NotFound {
            entity: "Session",
            criteria: "sessionIdOrToken=<redacted>".to_owned(),
        };
        let rendered = err.to_string();
        assert!(!rendered.contains("cqjc7ec6bpy65jjamstkhpe6fr"));
        assert!(rendered.contains("<redacted>"));
    }
}
