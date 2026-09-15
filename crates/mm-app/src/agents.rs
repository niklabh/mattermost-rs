//! Port of the agents-bridge status half of `app/agents.go` and `app/agents_bridge.go` —
//! `GetAIPluginBridgeStatus` (:30), `GetAgents` (:35), `GetLLMServices` (:83) and the live
//! bridge's `getLiveStatus` (agents_bridge.go:106) — as far as a server with no plugin host can
//! answer them.
//!
//! # Why a server that hosts no plugins can answer at all
//!
//! `getLiveStatus` asks three questions in order: is the plugin environment initialised
//! (`PluginSettings.Enable`, since `GetPluginsEnvironment` is nil when it is off); is the
//! `mattermost-ai` plugin active; and is its manifest version at least 1.5.0. This server has no
//! plugin host, so the second question is always *no* — which is the truth about this process,
//! and also what the stack's Go answers (measured: `plugin_not_active`). The list routes then
//! answer `[]` without a bridge call (`GetAgents`, agents_bridge.go:62). What this side cannot
//! know is whether the Go server beside it has the plugin installed; a deployment that installs
//! it there would see agents from Go and none from here. Recorded here, not guarded, because a
//! guard would forward every request on the strength of a plugin nobody has installed.

use mm_model::agents::{BridgeAgentInfo, BridgeServiceInfo};
use mm_model::utils::AppResult;

use crate::App;

/// `getLiveStatus`'s reason when `PluginSettings.Enable` is off (agents_bridge.go:110).
pub const REASON_PLUGIN_ENV_NOT_INITIALIZED: &str =
    "app.agents.bridge.not_available.plugin_env_not_initialized";
/// `getLiveStatus`'s reason when the `mattermost-ai` plugin is not active (:116).
pub const REASON_PLUGIN_NOT_ACTIVE: &str = "app.agents.bridge.not_available.plugin_not_active";

impl App {
    /// Port of `App.GetAIPluginBridgeStatus` (app/agents.go:30) — `(available, reason)`.
    ///
    /// Never available here (see the module docs); the reason is the first of `getLiveStatus`'s
    /// three that applies, and only the first two can.
    pub fn ai_plugin_bridge_status(&self) -> (bool, &'static str) {
        if !self.config().plugin_enable {
            return (false, REASON_PLUGIN_ENV_NOT_INITIALIZED);
        }
        (false, REASON_PLUGIN_NOT_ACTIVE)
    }

    /// Port of `App.GetAgents` (app/agents.go:35): the bridge answers `[]` when it is not
    /// available, before any call is made — and it is never available here.
    pub fn agents(&self) -> AppResult<Vec<BridgeAgentInfo>> {
        Ok(Vec::new())
    }

    /// Port of `App.GetLLMServices` (app/agents.go:83); same shape as [`App::agents`].
    pub fn llm_services(&self) -> AppResult<Vec<BridgeServiceInfo>> {
        Ok(Vec::new())
    }
}
