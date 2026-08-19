//! Port of `SqlUserStore` (channels/store/sqlstore/user_store.go), `Get` only.

use mm_model::user::User;
use mm_model::utils::{StringArray, StringMap};
use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `store.UserStore` (store/store.go:448-550) that is ported.
pub trait UserStore {
    /// Port of `SqlUserStore.Get` (user_store.go:609).
    fn get(&self, id: &str) -> impl std::future::Future<Output = Result<User, StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlUserStore {
    pool: PgPool,
}

impl SqlUserStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl UserStore for SqlUserStore {
    #[tracing::instrument(skip_all, fields(user_id = %id, found))]
    async fn get(&self, id: &str) -> Result<User, StoreError> {
        // Go builds this as `usersQuery.Where("Id = ?", id)`, where `usersQuery` is
        // `Select(getUsersColumns()) + Columns(getBotInfoColumns()) FROM Users
        //  LEFT JOIN Bots b ON (b.UserId = Users.Id)` (user_store.go:120-126).
        //
        // The LEFT JOIN is not optional decoration: `is_bot` is `b.UserId IS NOT NULL`, so
        // dropping the join would make every user a non-bot — including the bots. The two
        // COALESCEs are Go's, reproduced rather than replaced with Rust-side defaulting, so the
        // database answers the same question for both servers.
        //
        // `failedattempts` is `integer` in the schema and `int64` on the model, hence the cast.
        let row = sqlx::query!(
            r#"
            SELECT u.id,
                   u.createat,
                   u.updateat,
                   u.deleteat,
                   u.username,
                   u.password,
                   u.authdata,
                   u.authservice,
                   u.email,
                   u.emailverified,
                   u.nickname,
                   u.firstname,
                   u.lastname,
                   u.position,
                   u.roles,
                   u.allowmarketing,
                   u.props,
                   u.notifyprops,
                   u.lastpasswordupdate,
                   u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts,
                   u.locale,
                   u.timezone,
                   u.mfaactive,
                   u.mfasecret,
                   u.mfausedtimestamps,
                   u.remoteid,
                   u.lastlogin,
                   (b.userid IS NOT NULL) AS "isbot!",
                   COALESCE(b.description, '') AS "botdescription!",
                   COALESCE(b.lasticonupdate, 0) AS "botlasticonupdate!"
              FROM users u
              LEFT JOIN bots b ON b.userid = u.id
             WHERE u.id = $1
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get User with userId={id}"),
            source,
        })?;

        let Some(row) = row else {
            tracing::Span::current().record("found", false);
            // Go interpolates the id here and it is not a credential, so this one matches.
            return Err(StoreError::NotFound {
                entity: "User",
                criteria: id.to_owned(),
            });
        };
        tracing::Span::current().record("found", true);

        // Go unmarshals these three unconditionally and returns the error, so a malformed column
        // is a failed request on both sides rather than a silently empty map.
        //
        // **A JSON `null` is not malformed.** These columns are `jsonb`, which can hold the JSON
        // value `null` as distinct from SQL NULL, and the Go server writes exactly that: four of
        // the five users in the development database have `mfausedtimestamps = 'null'::jsonb`.
        // Go's `json.Unmarshal` turns a JSON null into a nil map or slice without complaint, so
        // both null shapes mean "absent" and only a *type* mismatch is an error. Treating JSON
        // null as a decode failure made `GET /users/me` a 500 for every user except the one the
        // parity tests happen to log in as — see [D-135].
        let decode_map = |value: Option<serde_json::Value>,
                          column: &'static str|
         -> Result<Option<StringMap>, StoreError> {
            match value {
                None | Some(serde_json::Value::Null) => Ok(None),
                Some(value) => Ok(Some(serde_json::from_value::<StringMap>(value).map_err(
                    |source| StoreError::Decode {
                        entity: "User",
                        column,
                        source,
                    },
                )?)),
            }
        };

        let mfa_used_timestamps = match row.mfausedtimestamps {
            None | Some(serde_json::Value::Null) => None,
            Some(value) => Some(serde_json::from_value::<StringArray>(value).map_err(
                |source| StoreError::Decode {
                    entity: "User",
                    column: "mfausedtimestamps",
                    source,
                },
            )?),
        };

        Ok(User {
            id: row.id,
            create_at: row.createat.unwrap_or_default(),
            update_at: row.updateat.unwrap_or_default(),
            delete_at: row.deleteat.unwrap_or_default(),
            username: row.username.unwrap_or_default(),
            password: row.password.unwrap_or_default(),
            auth_data: row.authdata,
            auth_service: row.authservice.unwrap_or_default(),
            email: row.email.unwrap_or_default(),
            email_verified: row.emailverified.unwrap_or_default(),
            nickname: row.nickname.unwrap_or_default(),
            first_name: row.firstname.unwrap_or_default(),
            last_name: row.lastname.unwrap_or_default(),
            position: row.position.unwrap_or_default(),
            roles: row.roles.unwrap_or_default(),
            allow_marketing: row.allowmarketing.unwrap_or_default(),
            props: decode_map(row.props, "props")?,
            notify_props: decode_map(row.notifyprops, "notifyprops")?,
            last_password_update: row.lastpasswordupdate.unwrap_or_default(),
            last_picture_update: row.lastpictureupdate.unwrap_or_default(),
            failed_attempts: row.failedattempts.unwrap_or_default(),
            locale: row.locale.unwrap_or_default(),
            timezone: decode_map(row.timezone, "timezone")?,
            mfa_active: row.mfaactive.unwrap_or_default(),
            mfa_secret: row.mfasecret.unwrap_or_default(),
            mfa_used_timestamps,
            remote_id: row.remoteid,
            last_login: row.lastlogin,
            is_bot: row.isbot,
            bot_description: row.botdescription,
            bot_last_icon_update: row.botlasticonupdate,

            // Not columns on `Users`, and Go's `Get` does not populate them either. Each is
            // filled by a different store or left zero:
            //   last_activity_at              — the `Status` table, via a separate query
            //   terms_of_service_*            — `UserTermsOfService`, which the api4 handler
            //                                   fetches separately (api4/user.go:329)
            //   disable_welcome_email         — request-scoped, never persisted
            last_activity_at: 0,
            terms_of_service_id: String::new(),
            terms_of_service_create_at: 0,
            disable_welcome_email: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_not_found_carries_the_id_go_puts_in_its_message() {
        let err = StoreError::NotFound {
            entity: "User",
            criteria: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
        };
        assert!(err.is_not_found());
        assert_eq!(
            err.to_string(),
            "User not found: y9i4er48tt8bukijy7i3u5y9ar"
        );
    }
}
