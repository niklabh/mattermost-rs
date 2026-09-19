//! Port of `SqlChannelGuardStore` (channels/store/sqlstore/channel_guard_store.go) — the read
//! half. A *channel guard* is a plugin's claim on a channel: while one exists, the two
//! `MessageWillBe*` hooks stop being fail-open advice and become a gate
//! (`app/guarded_hooks.go`, and [`mm_app::plugin_hooks`] on this side).
//!
//! # Only the reads
//!
//! `Save` and `Delete` exist for `RegisterChannelGuard` / `UnregisterChannelGuard`, two of the
//! 258 plugin API methods, which are the plugin plan's Phase 6 and answer the not-implemented
//! error here. Porting them now would be a store method no caller can reach; they land with the
//! API method that writes them.
//!
//! # Go reads a cache, this reads the table
//!
//! `Channels.reloadGuardCache` loads the whole table into a `sync.Map` at start-up and again on
//! every register, unregister and cluster invalidation (`app/channel_guards.go`). Nothing in
//! *this* process can write a guard, and nothing reloads the cache, so a cache here would only
//! be a way to serve a stale answer; [`ChannelGuardStore::get_for_channel`] is read per
//! dispatch instead. The observable difference is the other way round: a row written while Go
//! runs is invisible to Go until it reloads and visible here at once.

use sqlx::PgPool;

use crate::error::StoreError;

/// Port of `store.ChannelGuard` (store/store.go) — one plugin's claim on one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelGuard {
    pub channel_id: String,
    pub plugin_id: String,
    pub created_at: i64,
}

/// The read half of `store.ChannelGuardStore`.
pub trait ChannelGuardStore {
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
