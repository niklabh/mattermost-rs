//! Port of `SqlRemoteClusterStore` (channels/store/sqlstore/remote_cluster_store.go), narrowed to
//! `Get` — the one method `GetRemoteClusterSession` reaches, which is how a remote server's
//! `X-RemoteCluster-Token` is resolved for `POST /api/v4/remotecluster/{user_id}/image`.
//!
//! # A NULL column is a failed lookup, not an empty field
//!
//! Go scans the row with `sqlx.Get` into `model.RemoteCluster`, whose fields are plain `string`,
//! `int64` and `uint32`. `database/sql` refuses to convert a NULL into any of those, so a row with
//! a NULL in a nullable column (`RemoteTeamId`, `DisplayName`, `SiteURL`, `CreateAt`, `LastPingAt`,
//! `Token`, `RemoteToken`, `Topics`, `CreatorId`, `DefaultTeamId`, `DeleteAt`,
//! `LastGlobalUserSyncAt`) is an **error** from `Get`, and the session built on it is refused with
//! the same `invalid_token` 401 as a wrong token — measured on the licensed oracle with
//! `RemoteTeamId` set to NULL. `Save` always writes every column, so only a row planted by hand
//! has one; the `!` overrides below make sqlx fail on it the same way. A negative `Options` is
//! the same failure: Go scans the `smallint` into a `uint32` and `strconv.ParseUint` refuses it.

use mm_model::remote_cluster::{Bitmask, RemoteCluster};
use sqlx::PgPool;

use crate::error::StoreError;

/// Port of `store.RemoteClusterStore`, narrowed to what the remote-cluster session reaches.
pub trait RemoteClusterStore {
    /// Port of `sqlRemoteClusterStore.Get` (remote_cluster_store.go:174): the row for
    /// `remote_id`, filtered to `DeleteAt = 0` unless `include_deleted`.
    ///
    /// A missing row is [`StoreError::NotFound`] (Go's `sql.ErrNoRows`, which the app layer turns
    /// into a 404); any other failure — including a NULL column, see the module docs — is
    /// [`StoreError::Db`].
    fn get(
        &self,
        remote_id: &str,
        include_deleted: bool,
    ) -> impl std::future::Future<Output = Result<RemoteCluster, StoreError>> + Send;
}

#[derive(Debug, Clone)]
pub struct SqlRemoteClusterStore {
    pool: PgPool,
}

impl SqlRemoteClusterStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl RemoteClusterStore for SqlRemoteClusterStore {
    #[tracing::instrument(skip(self), fields(remote_id = %remote_id))]
    async fn get(
        &self,
        remote_id: &str,
        include_deleted: bool,
    ) -> Result<RemoteCluster, StoreError> {
        // `remoteClusterFields("")`, every one of them: selecting only `Token` would accept the
        // NULL-column rows Go's scan refuses. `(deleteat = 0 OR $2)` is `sq.Eq{"DeleteAt": 0}`
        // added only when `!includeDeleted` — a NULL `DeleteAt` fails the filter, as in Go.
        let row = sqlx::query!(
            r#"
            SELECT remoteid AS "remote_id!",
                   remoteteamid AS "remote_team_id!",
                   name AS "name!",
                   displayname AS "display_name!",
                   siteurl AS "site_url!",
                   defaultteamid AS "default_team_id!",
                   createat AS "create_at!",
                   deleteat AS "delete_at!",
                   lastpingat AS "last_ping_at!",
                   token AS "token!",
                   remotetoken AS "remote_token!",
                   topics AS "topics!",
                   creatorid AS "creator_id!",
                   pluginid AS "plugin_id!",
                   options AS "options!",
                   lastglobalusersyncat AS "last_global_user_sync_at!"
              FROM remoteclusters
             WHERE remoteid = $1 AND (deleteat = 0 OR $2)
            "#,
            remote_id,
            include_deleted,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find RemoteCluster".to_owned(),
            source,
        })?
        .ok_or_else(|| StoreError::NotFound {
            entity: "RemoteCluster",
            criteria: format!("remoteId={remote_id}"),
        })?;

        let options = u32::try_from(row.options).map_err(|err| StoreError::Db {
            context: "failed to find RemoteCluster".to_owned(),
            source: sqlx::Error::Decode(Box::new(err)),
        })?;

        Ok(RemoteCluster {
            remote_id: row.remote_id,
            remote_team_id: row.remote_team_id,
            name: row.name,
            display_name: row.display_name,
            site_url: row.site_url,
            default_team_id: row.default_team_id,
            create_at: row.create_at,
            delete_at: row.delete_at,
            last_ping_at: row.last_ping_at,
            last_global_user_sync_at: row.last_global_user_sync_at,
            token: row.token,
            remote_token: row.remote_token,
            topics: row.topics,
            creator_id: row.creator_id,
            plugin_id: row.plugin_id,
            options: Bitmask(options),
        })
    }
}
