//! Port of `model/plugin_status.go` — a cluster-aware view of one installed plugin.

use serde::{Deserialize, Serialize};

/// Port of `model.PluginStateNotRunning` (plugin_status.go:6).
pub const PLUGIN_STATE_NOT_RUNNING: i64 = 0;
/// Port of `model.PluginStateStarting` (plugin_status.go:7). Unused by the server.
pub const PLUGIN_STATE_STARTING: i64 = 1;
/// Port of `model.PluginStateRunning` (plugin_status.go:8).
pub const PLUGIN_STATE_RUNNING: i64 = 2;
/// Port of `model.PluginStateFailedToStart` (plugin_status.go:9).
pub const PLUGIN_STATE_FAILED_TO_START: i64 = 3;
/// Port of `model.PluginStateFailedToStayRunning` (plugin_status.go:10).
pub const PLUGIN_STATE_FAILED_TO_STAY_RUNNING: i64 = 4;
/// Port of `model.PluginStateStopping` (plugin_status.go:11). Unused by the server.
pub const PLUGIN_STATE_STOPPING: i64 = 5;

/// Port of `model.PluginStatus` (plugin_status.go:15).
///
/// Eight fields, none with `omitempty`, so every key is always present — including `error`, which
/// is `""` on a healthy plugin rather than absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginStatus {
    #[serde(rename = "plugin_id")]
    pub plugin_id: String,

    #[serde(rename = "cluster_id")]
    pub cluster_id: String,

    #[serde(rename = "plugin_path")]
    pub plugin_path: String,

    /// One of the `PLUGIN_STATE_*` constants.
    #[serde(rename = "state")]
    pub state: i64,

    #[serde(rename = "error")]
    pub error: String,

    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "description")]
    pub description: String,

    #[serde(rename = "version")]
    pub version: String,
}

/// Port of `model.PluginStatuses` (plugin_status.go:26) — `[]*PluginStatus`, so `null` when nil.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PluginStatuses(pub Vec<PluginStatus>);

#[cfg(test)]
mod wire_parity {
    use super::*;

    /// Round-trips the Go-generated fixture: decode into the port's type, re-encode, and compare
    /// the value graphs. The fixture is produced by `reference/dump`, whose reflective filler
    /// gives **every** field a distinctive non-zero value — so a dropped key, a renamed tag or a
    /// mis-typed field cannot pass. This is the parity oracle, not a smoke test.
    macro_rules! assert_fixture_round_trips {
        ($ty:ty, $fixture:literal) => {{
            let raw = include_str!(concat!("../../../fixtures/", $fixture, ".json"));
            let decoded: $ty =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("decoding {}: {e}", $fixture));
            let expected: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(
                serde_json::to_value(&decoded).unwrap(),
                expected,
                "re-encoding {} does not match Go",
                $fixture
            );
        }};
    }

    #[test]
    fn plugin_status_round_trips_the_fixture() {
        assert_fixture_round_trips!(PluginStatus, "plugin_status");
    }
}
