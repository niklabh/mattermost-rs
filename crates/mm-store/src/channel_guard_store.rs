//! Port of `SqlChannelGuardStore` (channels/store/sqlstore/channel_guard_store.go). A *channel
//! guard* is a plugin's claim on a channel: while one exists, the two `MessageWillBe*` hooks stop
//! being fail-open advice and become a gate (`app/guarded_hooks.go`, and [`mm_app::plugin_hooks`]
//! on this side). The writes are `RegisterChannelGuard` / `UnregisterChannelGuard`'s
//! (`mm_app::channel_guards`); what reads [`ChannelGuardStore::get_all`] into a cache, and when,
//! is there too.

use sqlx::PgPool;

use crate::error::StoreError;

/// Port of `store.ChannelGuard` (store/store.go) — one plugin's claim on one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelGuard {
    pub channel_id: String,
    pub plugin_id: String,
    pub created_at: i64,
}

/// Port of `store.ChannelGuardStore`.
pub trait ChannelGuardStore {
    /// Port of `SqlChannelGuardStore.Save` (channel_guard_store.go:30): an insert that leaves an
    /// existing `(ChannelId, PluginId)` claim, and its `CreatedAt`, as they were.
    fn save(
        &self,
        guard: &ChannelGuard,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlChannelGuardStore.Delete` (channel_guard_store.go:44): the rows it removed,
    /// matching both columns, so another plugin's claim on the channel stays.
    fn delete(
        &self,
        channel_id: &str,
        plugin_id: &str,
    ) -> impl std::future::Future<Output = Result<u64, StoreError>> + Send;

    /// Port of `SqlChannelGuardStore.GetAll` (channel_guard_store.go:76), in no particular order.
    fn get_all(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<ChannelGuard>, StoreError>> + Send;

    /// Port of `SqlChannelGuardStore.GetForChannel` (channel_guard_store.go:65).
    fn get_for_channel(
        &self,
        channel_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<ChannelGuard>, StoreError>> + Send;
}

#[derive(Debug, Clone)]
pub struct SqlChannelGuardStore {
    pool: PgPool,
}

impl SqlChannelGuardStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl ChannelGuardStore for SqlChannelGuardStore {
    #[tracing::instrument(skip(self))]
    async fn save(&self, guard: &ChannelGuard) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO ChannelGuards (ChannelId, PluginId, CreatedAt) VALUES ($1, $2, $3)
             ON CONFLICT (ChannelId, PluginId) DO NOTHING",
            guard.channel_id,
            guard.plugin_id,
            guard.created_at
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to save channel guard for channel={} plugin={}",
                guard.channel_id, guard.plugin_id
            ),
            source,
        })?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn delete(&self, channel_id: &str, plugin_id: &str) -> Result<u64, StoreError> {
        let done = sqlx::query!(
            "DELETE FROM ChannelGuards WHERE ChannelId = $1 AND PluginId = $2",
            channel_id,
            plugin_id
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to delete channel guard for channel={channel_id} plugin={plugin_id}"
            ),
            source,
        })?;
        Ok(done.rows_affected())
    }

    #[tracing::instrument(skip(self))]
    async fn get_all(&self) -> Result<Vec<ChannelGuard>, StoreError> {
        let rows = sqlx::query!(
            r#"SELECT ChannelId AS "channel_id!", PluginId AS "plugin_id!", CreatedAt AS "created_at!"
               FROM ChannelGuards"#
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to get all channel guards".to_owned(),
            source,
        })?;
        Ok(rows
            .into_iter()
            .map(|r| ChannelGuard {
                channel_id: r.channel_id,
                plugin_id: r.plugin_id,
                created_at: r.created_at,
            })
            .collect())
    }

    #[tracing::instrument(skip(self))]
    async fn get_for_channel(&self, channel_id: &str) -> Result<Vec<ChannelGuard>, StoreError> {
        let rows = sqlx::query!(
            r#"SELECT ChannelId AS "channel_id!", PluginId AS "plugin_id!", CreatedAt AS "created_at!"
               FROM ChannelGuards WHERE ChannelId = $1"#,
            channel_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get channel guards for channel={channel_id}"),
            source,
        })?;
        Ok(rows
            .into_iter()
            .map(|r| ChannelGuard {
                channel_id: r.channel_id,
                plugin_id: r.plugin_id,
                created_at: r.created_at,
            })
            .collect())
    }
}
