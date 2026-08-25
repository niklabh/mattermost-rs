//! Port of `model/plugin_reattach.go` — the serialisable form of go-plugin's `ReattachConfig`,
//! used when the server re-attaches to an already-running plugin process (local development).
//!
//! # Not ported
//!
//! `NewPluginReattachConfig` and `ToHashicorpPluginReattachmentConfig` convert to and from
//! `github.com/hashicorp/go-plugin`'s own type. There is no such type on the Rust side, so the
//! conversions have no counterpart; the struct itself is what crosses the process boundary.

use crate::manifest::Manifest;
use crate::utils::{AppError, AppResult};

/// Port of `net.UnixAddr` as `PluginReattachConfig` uses it — a path plus a network name
/// (`unix`, `unixgram`, `unixpacket`). Not a Mattermost type; declared here because Rust's
/// `std::os::unix::net` has no address type that carries the network.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnixAddr {
    /// `Name` in Go — the socket path.
    pub name: String,
    /// `Net` in Go.
    pub net: String,
}

/// Port of `model.PluginReattachConfig` (plugin_reattach.go:12). No `json:` tags: Go relies on
/// the field names, so the wire keys are `Protocol`, `ProtocolVersion`, `Addr`, `Pid`, `Test`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginReattachConfig {
    pub protocol: String,
    pub protocol_version: i64,
    pub addr: UnixAddr,
    pub pid: i64,
    pub test: bool,
}

/// Port of `model.PluginReattachRequest` (plugin_reattach.go:45).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginReattachRequest {
    pub manifest: Option<Manifest>,
    pub plugin_reattach_config: Option<PluginReattachConfig>,
}

impl PluginReattachRequest {
    /// Port of `(*PluginReattachRequest).IsValid` (plugin_reattach.go:50).
    ///
    /// The error ids are `plugin_reattach_request.is_valid.*` — **no `model.` prefix**, unlike
    /// every other validator in the package.
    pub fn is_valid(&self) -> AppResult {
        if self.manifest.is_none() {
            return Err(err("manifest"));
        }
        if self.plugin_reattach_config.is_none() {
            return Err(err("plugin_reattach_config"));
        }

        Ok(())
    }
}

fn err(field: &str) -> Box<AppError> {
    Box::new(AppError::new(
        "PluginReattachRequest.IsValid",
        format!("plugin_reattach_request.is_valid.{field}.app_error"),
        None,
        "",
        400,
    ))
}
