//! Saving a configuration change through the Go server.
//!
//! Go holds its configuration in memory and its database store has no watcher, so a row this
//! process wrote would reach Go only when something asked it to reload; and `SetDefaults` and
//! `IsValid` are not ported ([D-700]). So a write this server must make — the plugin host's
//! `PluginStates` — is sent to Go as a `PATCH /config`, which validates and persists it and swaps
//! Go's copy; this process then reloads the row (`App::refresh_config`), which runs the plugin
//! host's config listener as Go's `SaveConfig` runs its own.
//!
//! **A patch replaces a map it names**: `config.Merge` does not merge map keys, so a patch of
//! `PluginStates` must carry the whole map, and `SetDefaults` then puts back the four default ids
//! if they are missing. Callers send the whole map.

use std::future::Future;
use std::pin::Pin;

/// Why the peer did not save a patch.
#[derive(Debug, thiserror::Error)]
#[error("the Go server did not save the configuration patch: {0}")]
pub struct PeerConfigError(pub String);

/// A boxed future, because the trait is held as `dyn`.
pub type PeerConfigFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), PeerConfigError>> + Send + 'a>>;

/// Something that can ask the Go server to save a configuration patch.
pub trait PeerConfig: Send + Sync + std::fmt::Debug {
    /// `PUT /api/v4/config/patch` with `patch`, as a system administrator.
    fn patch_config<'a>(&'a self, patch: &'a serde_json::Value) -> PeerConfigFuture<'a>;
}
