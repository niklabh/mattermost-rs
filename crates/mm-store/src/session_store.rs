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

    /// Port of `SqlSessionStore.Save` (session_store.go:41).
    ///
    /// # `PreSave` runs inside the store, and it overwrites
    ///
    /// Go mutates the session it is handed: `Id` and `Token` are filled **only if empty**, but
    /// `CreateAt` and `LastActivityAt` are assigned unconditionally — so a caller that set
    /// `CreateAt` has it discarded. `DoLogin` then reads `session.CreateAt` back out to pass to
    /// `UpdateLastLogin`, which is why the login timestamp is the store's clock and not the
    /// handler's.
    ///
    /// A non-empty `Id` is refused outright (`ErrInvalidInput`), which the app layer turns into
    /// `app.session.save.existing.app_error` at **400** while every other failure is
    /// `app.session.save.app_error` at 500.
    ///
    /// # The returned session is not the one you passed
    ///
    /// `TeamMembers` is populated from `Team().GetTeamsForUser(..., true)` and filtered to
    /// `DeleteAt == 0` — the same hydration [`SessionStore::get`] does, so a session handed
    /// straight back to a client carries its teams.
    fn save(
        &self,
        session: Session,
    ) -> impl std::future::Future<Output = Result<Session, StoreError>> + Send;

    /// Port of `SqlSessionStore.GetLRUSessions` (session_store.go:161).
    ///
    /// `ORDER BY LastActivityAt DESC` with a limit **and an offset** — so it returns the
    /// *least* recently used by skipping the newest `offset`. `limitNumberOfSessions`
    /// (app/session.go:155) calls it with limit 100 and offset 499 and revokes everything it
    /// gets back, which is how a user is capped at 500 live sessions.
    ///
    /// Unlike [`SessionStore::get_sessions`] this does **not** hydrate `TeamMembers`: Go's
    /// `GetLRUSessions` skips the loop `GetSessions` runs, and its only caller throws the
    /// sessions away after reading their ids.
    fn get_lru_sessions(
        &self,
        user_id: &str,
        limit: i64,
        offset: i64,
    ) -> impl std::future::Future<Output = Result<Vec<Session>, StoreError>> + Send;

    /// Port of `SqlSessionStore.GetSessions` (session_store.go:126).
    fn get_sessions(
        &self,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<Session>, StoreError>> + Send;

    /// Port of `SqlSessionStore.UpdateLastActivityAt` (session_store.go:323).
    ///
    /// The first **write** in this store, and the reason it exists is that `LastActivityAt` is
    /// shared state: Go's idle-timeout check revokes a session whose value is stale, so a request
    /// this server answers without refreshing it moves a live user closer to being logged out by
    /// the process next door. See [`SessionStore::remove`], which is the other half.
    fn update_last_activity_at(
        &self,
        session_id: &str,
        time: i64,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlSessionStore.Remove` (session_store.go:290).
    ///
    /// Go's parameter really is "id **or** token" and the statement compares the one value
    /// against both columns — the same shape as [`SessionStore::get`], and for the same reason:
    /// callers hold one or the other and the store does not care which.
    fn remove(
        &self,
        session_id_or_token: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlSessionStore.RemoveAllSessions` (session_store.go:298).
    ///
    /// `DELETE FROM Sessions` with no `WHERE`. There is exactly one caller — the
    /// sysadmin-only `POST /api/v4/users/sessions/revoke/all` — and it logs out **every user on
    /// the server, including the caller**. No id, no filter, no soft delete.
    fn remove_all_sessions(
        &self,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlSessionStore.UpdateDeviceId` (session_store.go:343).
    ///
    /// Four columns in one statement, and the fourth is the one a reader drops: `ExpiredNotify`
    /// is reset to `false` alongside the new `ExpiresAt`. Go writes them together because the
    /// flag records that the *old* expiry was already announced to the client; leaving it set
    /// against a fresh expiry suppresses the next warning.
    ///
    /// **Both device columns are always written.** The caller is responsible for passing the
    /// existing value back when it is only changing one of them — see `attachDeviceIds`
    /// (api4/user.go:2810), which reads the current session to do exactly that. Omitting that
    /// fallback here would be a silent wipe of the other column.
    fn update_device_id(
        &self,
        session_id: &str,
        device_id: &str,
        voip_device_id: &str,
        expires_at: i64,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlSessionStore.UpdateProps` (session_store.go:353).
    ///
    /// Writes the **whole** `Props` object, not a merge — the caller mutates the session it holds
    /// and passes it back. A nil map marshals to JSON `null` in Go rather than `{}`, and that is
    /// reproduced: see [D-331], which is the same column shape read back.
    fn update_props(
        &self,
        session: &Session,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlSessionStore.UpdateRoles` (session_store.go:331).
    ///
    /// **Every live session of the user**, not one row: the `WHERE` is on `UserId`, so a role
    /// change reaches the caller's phone and desktop as well as the browser that made it. A port
    /// keyed on `Id` would leave every other device carrying the old roles until it re-logged in.
    ///
    /// The length guard is `> UserRolesMaxLength` on the *bytes* of the string and it fires
    /// **before** the statement, so an over-long list writes nothing at all. It is a plain
    /// `fmt.Errorf` in Go rather than an `ErrInvalidInput`, and `UpdateUserRolesWithUser` only
    /// *logs* whatever comes back — so this failing is silent to the client either way, and the
    /// user row has already been written by then.
    fn update_roles(
        &self,
        user_id: &str,
        roles: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;
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

    #[tracing::instrument(skip_all, fields(session_id, user_id = %session.user_id))]
    async fn save(&self, mut session: Session) -> Result<Session, StoreError> {
        if !session.id.is_empty() {
            return Err(StoreError::InvalidInput {
                entity: "Session",
                field: "id",
                value: session.id,
            });
        }

        session.pre_save();
        session
            .is_valid()
            .map_err(|app_error| StoreError::Invalid {
                entity: "Session",
                app_error,
            })?;

        // `json.Marshal(session.Props)`. `pre_save` guarantees a map, so this is never `null`
        // from here — but the column is nullable and `Get` reads a NULL back as an empty map,
        // which is the shape [D-331] describes.
        // `Decode` for a *serialise*, as `update_props` below already does and for the same
        // reason: the direction is infallible in practice and a second variant for it is noise.
        let props = serde_json::to_value(&session.props).map_err(|source| StoreError::Decode {
            entity: "Session",
            column: "props",
            source,
        })?;

        sqlx::query!(
            r#"
            INSERT INTO sessions
                (id, token, createat, expiresat, lastactivityat, userid, deviceid,
                 voipdeviceid, roles, isoauth, expirednotify, props)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            "#,
            session.id,
            session.token,
            session.create_at,
            session.expires_at,
            session.last_activity_at,
            session.user_id,
            session.device_id,
            session.voip_device_id,
            session.roles,
            session.is_oauth,
            session.expired_notify,
            props,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to save Session with id={}", session.id),
            source,
        })?;

        tracing::Span::current().record("session_id", session.id.as_str());
        session.team_members = Some(self.team_members_for_session(&session.user_id).await?);
        Ok(session)
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, limit, offset, count))]
    async fn get_lru_sessions(
        &self,
        user_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Session>, StoreError> {
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
             LIMIT $2 OFFSET $3
            "#,
            user_id,
            limit,
            offset,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find Sessions with userId={user_id}"),
            source,
        })?;

        let sessions = rows
            .into_iter()
            .map(SessionRow::into_session)
            .collect::<Result<Vec<_>, StoreError>>()?;

        tracing::Span::current().record("limit", limit);
        tracing::Span::current().record("offset", offset);
        tracing::Span::current().record("count", sessions.len());
        Ok(sessions)
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

    #[tracing::instrument(skip_all, fields(session_id = %session_id, time = time))]
    async fn update_last_activity_at(&self, session_id: &str, time: i64) -> Result<(), StoreError> {
        // `UPDATE Sessions SET LastActivityAt = ? WHERE Id = ?` verbatim. **Id only** — unlike
        // `Get` and `Remove`, this one does not also match `Token`, so passing a token here
        // updates nothing and reports success. Go has the same hole; the caller holds a whole
        // session and passes `session.Id`.
        sqlx::query!(
            "UPDATE sessions SET lastactivityat = $1 WHERE id = $2",
            time,
            session_id
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update Session with id={session_id}"),
            source,
        })?;

        Ok(())
    }

    #[tracing::instrument(skip_all, fields(deleted))]
    async fn remove(&self, session_id_or_token: &str) -> Result<(), StoreError> {
        // `DELETE FROM Sessions WHERE Id = ? Or Token = ?`, one bind against two columns.
        //
        // Go ignores the affected-row count and so does this: removing a session that is already
        // gone is the success case, not a miss. Returning `NotFound` here would turn the revoke
        // half of an idle-timeout rejection into a 500 the moment two requests raced.
        let result = sqlx::query!(
            "DELETE FROM sessions WHERE id = $1 OR token = $1",
            session_id_or_token
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to delete Session by id or token".to_owned(),
            source,
        })?;

        tracing::Span::current().record("deleted", result.rows_affected());
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(deleted))]
    async fn remove_all_sessions(&self) -> Result<(), StoreError> {
        // `DELETE FROM Sessions` — verbatim, no predicate. See the trait doc for what that means.
        let result = sqlx::query!("DELETE FROM sessions")
            .execute(&self.pool)
            .await
            .map_err(|source| StoreError::Db {
                context: "failed to delete all Sessions".to_owned(),
                source,
            })?;

        tracing::Span::current().record("deleted", result.rows_affected());
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(session_id = %session_id))]
    async fn update_device_id(
        &self,
        session_id: &str,
        device_id: &str,
        voip_device_id: &str,
        expires_at: i64,
    ) -> Result<(), StoreError> {
        // `UPDATE Sessions SET DeviceId = ?, VoIPDeviceId = ?, ExpiresAt = ?, ExpiredNotify =
        // false WHERE Id = ?`. `ExpiredNotify = false` is a literal in Go, not a parameter.
        sqlx::query!(
            "UPDATE sessions SET deviceid = $1, voipdeviceid = $2, expiresat = $3, \
             expirednotify = false WHERE id = $4",
            device_id,
            voip_device_id,
            expires_at,
            session_id
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update Session with id={session_id}"),
            source,
        })?;

        Ok(())
    }

    #[tracing::instrument(skip_all, fields(session_id = %session.id))]
    async fn update_props(&self, session: &Session) -> Result<(), StoreError> {
        // `json.Marshal(session.Props)`: `None` is JSON `null`, an empty map is `{}`. Both are
        // reachable — `Session::props` is `Option<StringMap>` precisely because the column is.
        // A `StringMap` cannot fail to serialise, but `to_value` is fallible and swallowing the
        // result would be the "never swallow an error you could type" rule broken for nothing.
        // `Decode` is the variant `user_store` already uses for the same infallible-in-practice
        // direction; a second variant for it would be noise.
        let props = serde_json::to_value(&session.props).map_err(|source| StoreError::Decode {
            entity: "Session",
            column: "props",
            source,
        })?;

        sqlx::query!(
            "UPDATE sessions SET props = $1 WHERE id = $2",
            props,
            session.id
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to update Session".to_owned(),
            source,
        })?;

        Ok(())
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    async fn update_roles(&self, user_id: &str, roles: &str) -> Result<(), StoreError> {
        // `len(roles)` in Go is bytes, not characters.
        if roles.len() > mm_model::user::USER_ROLES_MAX_LENGTH {
            return Err(StoreError::Argument {
                entity: "Session",
                detail: "given session roles length exceeds max storage limit",
            });
        }

        sqlx::query!(
            "UPDATE sessions SET roles = $1 WHERE userid = $2",
            roles,
            user_id
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update Session with userId={user_id}"),
            source,
        })?;

        Ok(())
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
