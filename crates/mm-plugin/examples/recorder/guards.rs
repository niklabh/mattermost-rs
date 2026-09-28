//! The hook recorder's guard script: `RegisterChannelGuard` and `UnregisterChannelGuard`, for
//! `parity::plugin_hooks`' notification and join tour (`mm_app::channel_guards`).
//!
//! A post whose message is `!guard-script <channel id>` registers the plugin's guard on that
//! channel, and `!unguard-script <channel id>` removes it; each first sends the two ids the host
//! refuses (empty, and malformed) and then the real one twice — a second register keeps the first
//! claim, a second unregister removes nothing.

use go_netrpc::Client;
use mm_plugin::wire::plugin::{
    Z_RegisterChannelGuardArgs, Z_RegisterChannelGuardReturns, Z_UnregisterChannelGuardArgs,
    Z_UnregisterChannelGuardReturns,
};
use serde_json::Value as Json;

use crate::core::call;

/// The prefix that registers.
pub const GUARD_SCRIPT: &str = "!guard-script ";
/// The prefix that unregisters.
pub const UNGUARD_SCRIPT: &str = "!unguard-script ";

pub async fn register(api: &Client, channel_id: &str) -> Vec<Json> {
    let mut out = Vec::new();
    for id in ["", "short", channel_id, channel_id] {
        call::<_, Z_RegisterChannelGuardReturns>(
            api,
            &mut out,
            "RegisterChannelGuard",
            Z_RegisterChannelGuardArgs { a: id.to_owned() },
        )
        .await;
    }
    out
}

pub async fn unregister(api: &Client, channel_id: &str) -> Vec<Json> {
    let mut out = Vec::new();
    for id in ["", "short", channel_id, channel_id] {
        call::<_, Z_UnregisterChannelGuardReturns>(
            api,
            &mut out,
            "UnregisterChannelGuard",
            Z_UnregisterChannelGuardArgs { a: id.to_owned() },
        )
        .await;
    }
    out
}
