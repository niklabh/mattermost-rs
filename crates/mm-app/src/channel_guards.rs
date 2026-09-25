//! Port of app/channel_guards.go: the channel-guard cache, and the two writes a plugin makes to it
//! (`RegisterChannelGuard`, `UnregisterChannelGuard`).
//!
//! # The cache is the Rust host's only
//!
//! Go loads the whole `ChannelGuards` table into a map when the server starts, reloads it after
//! each register and unregister (and on a cluster invalidation), and answers every guarded
//! dispatch from it. This server does the same **when it hosts the plugins**: then it is the only
//! process that can write a guard (Go runs none, docs/PLUGIN_PLAN.md D6), so the map is as fresh
//! as Go's own would be. When Go hosts them, a guard is written by Go, which cannot tell this
//! process to reload — there is no cluster bus between the two — so [`App::guards_for_channel`]
//! reads the table per dispatch instead, and sees Go's writes at once.
//!
//! A failed reload keeps the old map and starts **one** retry task (a second failure while it
//! runs does not start another), which retries after 1 s, doubling to at most 5 min, until a
//! reload succeeds. Go also stops it at shutdown; this process ends with its tasks.
//!
//! `broadcastChannelGuardInvalidation` sends to the cluster, which is nil on every build we run,
//! so it does nothing here either.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use mm_model::utils::{AppError, get_millis, is_valid_id};
use mm_store::StoreError;
use mm_store::channel_guard_store::{ChannelGuard, ChannelGuardStore};

use crate::App;

/// `guardCacheRetryInitialDelay`.
pub const RETRY_INITIAL_DELAY: Duration = Duration::from_secs(1);
/// `guardCacheRetryMaxDelay`.
pub const RETRY_MAX_DELAY: Duration = Duration::from_secs(5 * 60);

/// Go's `Channels.guardCache` and `guardCacheRetryInFlight`.
#[derive(Debug, Default)]
pub struct GuardCache {
    map: RwLock<Arc<HashMap<String, Vec<ChannelGuard>>>>,
    retry_in_flight: AtomicBool,
}

impl GuardCache {
    /// `getGuardsForChannel`: the cached guards, or none.
    pub fn for_channel(&self, channel_id: &str) -> Vec<ChannelGuard> {
        self.map
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(channel_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Replace the whole map at once, as `guardCache.Store(fresh)` does.
    fn replace(&self, guards: Vec<ChannelGuard>) {
        let mut grouped: HashMap<String, Vec<ChannelGuard>> = HashMap::new();
        for guard in guards {
            grouped
                .entry(guard.channel_id.clone())
                .or_default()
                .push(guard);
        }
        *self.map.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(grouped);
    }
}

/// The delay after a failed retry: doubled, and capped.
pub fn next_retry_delay(delay: Duration) -> Duration {
    (delay * 2).min(RETRY_MAX_DELAY)
}

impl App {
    fn guard_cache(&self) -> &GuardCache {
        self.plugin_host().guard_cache()
    }

    /// The guards on `channel_id`, as the guarded dispatchers read them: from the cache when this
    /// process hosts the plugins, from the table otherwise (see the module comment).
    pub(crate) async fn guards_for_channel(
        &self,
        channel_id: &str,
    ) -> Result<Vec<ChannelGuard>, StoreError> {
        if self.plugin_host().hosted() {
            return Ok(self.guard_cache().for_channel(channel_id));
        }
        self.store()
            .channel_guard()
            .get_for_channel(channel_id)
            .await
    }

    /// `Channels.reloadGuardCache`: read every guard and swap the map. On failure the old map
    /// stays.
    pub async fn reload_guard_cache(&self) -> Result<(), StoreError> {
        let guards = self.store().channel_guard().get_all().await?;
        self.guard_cache().replace(guards);
        Ok(())
    }

    /// The start-up load `NewChannels` makes (app/channels.go:244), with its retry on failure.
    /// Only a hosting process keeps the cache.
    pub async fn load_guard_cache(&self) {
        if !self.plugin_host().hosted() {
            return;
        }
        if let Err(err) = self.reload_guard_cache().await {
            tracing::warn!(clustered = false, error = %err, "Failed to load channel guard cache at startup; retry scheduled");
            self.schedule_guard_cache_reload_retry();
        }
    }

    /// `scheduleGuardCacheReloadRetry`: `true` when this call started the retry task, `false`
    /// when one was already running.
    pub fn schedule_guard_cache_reload_retry(&self) -> bool {
        if self
            .guard_cache()
            .retry_in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false;
        }
        let app = self.clone();
        tokio::spawn(async move {
            let mut delay = RETRY_INITIAL_DELAY;
            let mut attempt = 1;
            loop {
                tokio::time::sleep(delay).await;
                match app.reload_guard_cache().await {
                    Ok(()) => {
                        tracing::info!(attempt, "Channel guard cache reload retry succeeded");
                        break;
                    }
                    Err(err) => {
                        tracing::info!(attempt, error = %err, "Channel guard cache reload retry attempt failed; will retry");
                        delay = next_retry_delay(delay);
                        attempt += 1;
                    }
                }
            }
            app.guard_cache()
                .retry_in_flight
                .store(false, Ordering::SeqCst);
        });
        true
    }

    /// The reload after a write, as Register and Unregister run it: a failure is logged and
    /// retried, never the caller's error.
    async fn reload_after_write(&self, what: &str, channel_id: &str, plugin_id: &str) {
        if !self.plugin_host().hosted() {
            return;
        }
        if let Err(err) = self.reload_guard_cache().await {
            tracing::warn!(channel_id, plugin_id, error = %err, "Failed to reload channel guard cache after {what}; retry scheduled");
            self.schedule_guard_cache_reload_retry();
        }
    }

    /// Port of `App.RegisterChannelGuard` (app/channel_guards.go:100). `plugin_id` arrives
    /// lower-cased from the plugin API. An empty channel id and a malformed one are two
    /// different 400s; an existing claim is kept as it was.
    pub async fn register_channel_guard(
        &self,
        channel_id: &str,
        plugin_id: &str,
    ) -> Result<(), Box<AppError>> {
        check_channel_id("RegisterChannelGuard", "register", channel_id)?;
        let guard = ChannelGuard {
            channel_id: channel_id.to_owned(),
            plugin_id: plugin_id.to_owned(),
            created_at: get_millis(),
        };
        if let Err(err) = self.store().channel_guard().save(&guard).await {
            let detail = err.to_string();
            return Err(Box::new(
                AppError::new(
                    "RegisterChannelGuard",
                    "app.channel_guard.register.app_error",
                    None,
                    detail,
                    500,
                )
                .wrap(err),
            ));
        }
        self.reload_after_write("Register", channel_id, plugin_id)
            .await;
        Ok(())
    }

    /// Port of `App.UnregisterChannelGuard` (app/channel_guards.go:138). Removing a claim the
    /// plugin does not hold is not an error; it is logged.
    pub async fn unregister_channel_guard(
        &self,
        channel_id: &str,
        plugin_id: &str,
    ) -> Result<(), Box<AppError>> {
        check_channel_id("UnregisterChannelGuard", "unregister", channel_id)?;
        let removed = match self
            .store()
            .channel_guard()
            .delete(channel_id, plugin_id)
            .await
        {
            Ok(removed) => removed,
            Err(err) => {
                let detail = err.to_string();
                return Err(Box::new(
                    AppError::new(
                        "UnregisterChannelGuard",
                        "app.channel_guard.unregister.app_error",
                        None,
                        detail,
                        500,
                    )
                    .wrap(err),
                ));
            }
        };
        if removed == 0 {
            tracing::warn!(
                error_id = "unregister_no_matching_guard",
                channel_id,
                plugin_id,
                "UnregisterChannelGuard removed no rows; pluginID does not match any guard for this channel"
            );
        }
        self.reload_after_write("Unregister", channel_id, plugin_id)
            .await;
        Ok(())
    }
}

/// The two refusals Register and Unregister share, each under its own `where` and, for the empty
/// id, its own key.
fn check_channel_id(r#where: &str, verb: &str, channel_id: &str) -> Result<(), Box<AppError>> {
    if channel_id.is_empty() {
        return Err(AppError::boxed(
            r#where,
            format!("app.channel_guard.{verb}.empty_channel.app_error"),
            None,
            String::new(),
            400,
        ));
    }
    if !is_valid_id(channel_id) {
        return Err(AppError::boxed(
            r#where,
            "app.channel_guard.invalid_channel.app_error",
            None,
            String::new(),
            400,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard(channel: &str, plugin: &str) -> ChannelGuard {
        ChannelGuard {
            channel_id: channel.into(),
            plugin_id: plugin.into(),
            created_at: 1,
        }
    }

    #[test]
    fn the_cache_groups_by_channel_and_is_replaced_whole() {
        let cache = GuardCache::default();
        assert!(cache.for_channel("c1").is_empty());
        cache.replace(vec![guard("c1", "a"), guard("c2", "b"), guard("c1", "c")]);
        let ids: Vec<String> = cache
            .for_channel("c1")
            .into_iter()
            .map(|g| g.plugin_id)
            .collect();
        assert_eq!(ids, ["a", "c"]);
        cache.replace(vec![guard("c2", "b")]);
        assert!(cache.for_channel("c1").is_empty());
        assert_eq!(cache.for_channel("c2").len(), 1);
    }

    #[test]
    fn the_retry_delay_doubles_to_five_minutes() {
        assert_eq!(
            next_retry_delay(RETRY_INITIAL_DELAY),
            Duration::from_secs(2)
        );
        assert_eq!(
            next_retry_delay(Duration::from_secs(200)),
            Duration::from_secs(300)
        );
        assert_eq!(next_retry_delay(RETRY_MAX_DELAY), RETRY_MAX_DELAY);
    }

    #[test]
    fn the_refusals_are_gos() {
        let err = check_channel_id("RegisterChannelGuard", "register", "").unwrap_err();
        assert_eq!(
            (err.id.as_str(), err.where_.as_str(), err.status_code),
            (
                "app.channel_guard.register.empty_channel.app_error",
                "RegisterChannelGuard",
                400
            )
        );
        let err = check_channel_id("UnregisterChannelGuard", "unregister", "short").unwrap_err();
        assert_eq!(
            (err.id.as_str(), err.where_.as_str(), err.status_code),
            (
                "app.channel_guard.invalid_channel.app_error",
                "UnregisterChannelGuard",
                400
            )
        );
        assert!(
            check_channel_id(
                "RegisterChannelGuard",
                "register",
                "abcdefghijklmnopqrstuvwxyz"
            )
            .is_ok()
        );
    }
}
