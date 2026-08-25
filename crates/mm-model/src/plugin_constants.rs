//! Port of `model/plugin_constants.go` — the ids of the plugins the server special-cases.

/// Port of `model.PluginIdPlaybooks` (plugin_constants.go:6).
pub const PLUGIN_ID_PLAYBOOKS: &str = "playbooks";
/// Port of `model.PluginIdFocalboard` (plugin_constants.go:7).
pub const PLUGIN_ID_FOCALBOARD: &str = "focalboard";
/// Port of `model.PluginIdApps` (plugin_constants.go:8).
pub const PLUGIN_ID_APPS: &str = "com.mattermost.apps";
/// Port of `model.PluginIdCalls` (plugin_constants.go:9).
pub const PLUGIN_ID_CALLS: &str = "com.mattermost.calls";
/// Port of `model.PluginIdNPS` (plugin_constants.go:10).
pub const PLUGIN_ID_NPS: &str = "com.mattermost.nps";
/// Port of `model.PluginIdChannelExport` (plugin_constants.go:11).
pub const PLUGIN_ID_CHANNEL_EXPORT: &str = "com.mattermost.plugin-channel-export";
/// Port of `model.PluginIdAI` (plugin_constants.go:12) — note it is **not** reverse-DNS.
pub const PLUGIN_ID_AI: &str = "mattermost-ai";
