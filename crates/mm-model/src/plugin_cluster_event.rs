//! Port of `model/plugin_cluster_event.go` — intra-cluster plugin-to-plugin messages.

use crate::cluster_message::{CLUSTER_SEND_BEST_EFFORT, CLUSTER_SEND_RELIABLE};

/// Port of `model.PluginClusterEventSendTypeReliable` (plugin_cluster_event.go:6) — an alias of
/// [`CLUSTER_SEND_RELIABLE`], re-exported rather than re-transcribed so the two cannot drift.
pub const PLUGIN_CLUSTER_EVENT_SEND_TYPE_RELIABLE: &str = CLUSTER_SEND_RELIABLE;
/// Port of `model.PluginClusterEventSendTypeBestEffort` (plugin_cluster_event.go:7).
pub const PLUGIN_CLUSTER_EVENT_SEND_TYPE_BEST_EFFORT: &str = CLUSTER_SEND_BEST_EFFORT;

/// Port of `model.PluginClusterEvent` (plugin_cluster_event.go:11). No `json:` tags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginClusterEvent {
    pub id: String,
    pub data: Option<Vec<u8>>,
}

/// Port of `model.PluginClusterEventSendOptions` (plugin_cluster_event.go:19).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginClusterEventSendOptions {
    /// One of the two `PLUGIN_CLUSTER_EVENT_SEND_TYPE_*` constants.
    pub send_type: String,
    /// The cluster id of the receiving node. **Empty broadcasts to every other node** — so a
    /// caller that forgets to set it does not fail, it fans out.
    pub target_id: String,
}
