//! Port of `SqlSessionStore` (channels/store/sqlstore/session_store.go), `Get` and `GetSessions`.
//!
//! This is the Strangler Fig's load-bearing store. Both servers read the same `Sessions` rows, so
//! a token minted by the Go server has to authenticate here without a second login — which is
//! why this method, and not something easier, is the first one ported.

use mm_model::session::Session;
use mm_model::team_member::TeamMember;
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

    /// Port of `SqlSessionStore.GetSessions` (session_store.go:126).
    fn get_sessions(
        &self,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<Session>, StoreError>> + Send;
}

/// One row of `me.sessionSelectQuery`, named so both queries share a mapping.
///
/// `Sessions` has no NOT NULL constraint on anything but `Id` and `VoipDeviceId`, so everything
/// else is an `Option`. Go scans these into non-pointer struct fields, which means a real NULL
/// would be a scan error there; here it defaults. See [D-078].
struct SessionRow {
    id: String,
    token: Option<String>,
    createat: Option<i64>,
    expiresat: Option<i64>,
    lastactivityat: Option<i64>,
    userid: Option<String>,
    deviceid: Option<String>,
    voipdeviceid: String,
    roles: Option<String>,
    isoauth: Option<bool>,
    props: Option<serde_json::Value>,
    expirednotify: Option<bool>,
}

impl SessionRow {
    /// Map a row to the wire type. `team_members` is `db:"-"` in Go and hydrated separately.
    fn into_session(self) -> Result<Session, StoreError> {
        let props =
            match self.props {
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
            id: self.id,
            token: self.token.unwrap_or_default(),
            create_at: self.createat.unwrap_or_default(),
            expires_at: self.expiresat.unwrap_or_default(),
            last_activity_at: self.lastactivityat.unwrap_or_default(),
            user_id: self.userid.unwrap_or_default(),
            device_id: self.deviceid.unwrap_or_default(),
            // The only NOT NULL column besides `Id`: the v11 migration that added it gave it a
            // default, so sqlx types it `String` rather than `Option<String>`.
            voip_device_id: self.voipdeviceid,
            roles: self.roles.unwrap_or_default(),
            is_oauth: self.isoauth.unwrap_or_default(),
            expired_notify: self.expirednotify.unwrap_or_default(),
            props,
            team_members: None,
            local: false,
        })
    }
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

    /// The team members Go attaches to a session: every membership of the user, **minus the
    /// deleted ones**.
    ///
    /// Go passes `includeDeleted = true` to the store and then discards `DeleteAt != 0` in Go
    /// code (session_store.go:118, :148). Same result as filtering in SQL, reproduced as written
    /// because the difference becomes observable if either side is changed independently.
    ///
    /// The list is always `Some`, never `None`: Go writes `make([]*model.TeamMember, 0, n)`, so a
    /// user in no teams serialises `"team_members": []` rather than `null`.
    async fn team_members_for_session(&self, user_id: &str) -> Result<Vec<TeamMember>, StoreError> {
        let members = crate::team_store::get_teams_for_user(&self.pool, user_id, "", true)
            .await
            .map_err(|err| match err {
                // Go wraps this as "failed to find TeamMembers for Session with userId=%s". A
                // missing team member is not a missing session, so the not-found variant must
                // not escape here and become a 401 at the API edge.
                StoreError::NotFound { .. } => StoreError::Db {
                    context: format!(
                        "failed to find TeamMembers for Session with userId={user_id}"
                    ),
                    source: sqlx::Error::RowNotFound,
                },
                other => other,
            })?;

        Ok(members
            .into_iter()
            .filter(|member| member.delete_at == 0)
            .collect())
    }
}

impl SessionStore for SqlSessionStore {
    #[tracing::instrument(skip_all, fields(found))]
    async fn get(&self, session_id_or_token: &str) -> Result<Session, StoreError> {
        // Go builds this with squirrel as `sq.Or{sq.Eq{"Token": x}, sq.Eq{"Id": x}}` and
        // `Limit(1)`. One bind parameter covers both sides: the argument is compared against two
        // columns, not two arguments against one column each.
        let row = sqlx::query_as!(
            SessionRow,
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

        let mut session = row.into_session()?;
        session.team_members = Some(self.team_members_for_session(&session.user_id).await?);
        Ok(session)
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, count))]
    async fn get_sessions(&self, user_id: &str) -> Result<Vec<Session>, StoreError> {
        // `ORDER BY LastActivityAt DESC` is Go's, and it is part of the response: the API returns
        // this list verbatim, so the order is on the wire rather than an implementation detail.
        let rows = sqlx::query_as!(
            SessionRow,
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
             WHERE userid = $1
             ORDER BY lastactivityat DESC
            "#,
            user_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find Sessions with userId={user_id}"),
            source,
        })?;

        // One team-members query for the whole list, not one per session — Go does the same and
        // assigns the *same* members to every session (session_store.go:146-154). Ours clones per
        // session because `Session` owns its list where Go shares pointers; the clone is required
        // by the ownership model, not a workaround for the borrow checker.
        let members = self.team_members_for_session(user_id).await?;

        let sessions = rows
            .into_iter()
            .map(|row| {
                let mut session = row.into_session()?;
                session.team_members = Some(members.clone());
                Ok(session)
            })
            .collect::<Result<Vec<_>, StoreError>>()?;

        tracing::Span::current().record("count", sessions.len());
        Ok(sessions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn null_row() -> SessionRow {
        SessionRow {
            id: "sessionid".to_owned(),
            token: None,
            createat: None,
            expiresat: None,
            lastactivityat: None,
            userid: None,
            deviceid: None,
            voipdeviceid: String::new(),
            roles: None,
            isoauth: None,
            props: None,
            expirednotify: None,
        }
    }

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

    /// NULL in a nullable column becomes the zero value rather than a scan error. Go would fail
    /// here; being more permissive cannot produce a wrong non-empty value. See D-078.
    #[test]
    fn nulls_become_zero_values_rather_than_errors() {
        let session = null_row()
            .into_session()
            .expect("a row of NULLs still maps");
        assert_eq!(session.id, "sessionid");
        assert_eq!(session.token, "");
        assert_eq!(session.expires_at, 0);
        assert!(!session.is_oauth);
        assert_eq!(session.props, None);
        // Hydrated by the caller, never by the row.
        assert_eq!(session.team_members, None);
    }

    /// A `props` column holding something that is not a string map is an error on both sides,
    /// not a silently empty map.
    #[test]
    fn malformed_props_is_a_decode_error() {
        let mut row = null_row();
        // `StringMap` is map[string]string; a nested object cannot be one.
        row.props = Some(serde_json::json!({"nested": {"not": "a string"}}));

        let err = row.into_session().expect_err("this must not decode");
        assert!(matches!(
            err,
            StoreError::Decode {
                entity: "Session",
                column: "props",
                ..
            }
        ));
        assert!(!err.is_not_found(), "a decode failure is not a 401");
    }
}
