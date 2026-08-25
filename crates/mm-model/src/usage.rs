//! Port of `model/usage.go` — the four usage counters and the installed-integrations listing.

use serde::{Deserialize, Serialize};

/// Port of `model.PostsUsage` (usage.go:3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PostsUsage {
    #[serde(rename = "count")]
    pub count: i64,
}

/// Port of `model.StorageUsage` (usage.go:7).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageUsage {
    #[serde(rename = "bytes")]
    pub bytes: i64,
}

/// Port of `model.TeamsUsage` (usage.go:11).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TeamsUsage {
    #[serde(rename = "active")]
    pub active: i64,

    #[serde(rename = "cloud_archived")]
    pub cloud_archived: i64,
}

/// Port of `model.InstalledIntegrationsIgnoredPlugins` (usage.go:16) — the seven first-party
/// plugins that do not count as "integrations" for billing.
///
/// Go's `map[string]struct{}` is a set. Seven entries do not justify a `HashSet` built at
/// startup, so the Rust shape is a plain array plus [`is_ignored_plugin`], which scans it. The
/// order is Go's map-literal order and carries no meaning — the lookup does not depend on it.
pub const INSTALLED_INTEGRATIONS_IGNORED_PLUGINS: [&str; 7] = [
    crate::plugin_constants::PLUGIN_ID_APPS,
    crate::plugin_constants::PLUGIN_ID_CALLS,
    crate::plugin_constants::PLUGIN_ID_NPS,
    crate::plugin_constants::PLUGIN_ID_CHANNEL_EXPORT,
    crate::plugin_constants::PLUGIN_ID_FOCALBOARD,
    crate::plugin_constants::PLUGIN_ID_AI,
    crate::plugin_constants::PLUGIN_ID_PLAYBOOKS,
];

/// Whether a plugin id is one of [`INSTALLED_INTEGRATIONS_IGNORED_PLUGINS`].
pub fn is_ignored_plugin(plugin_id: &str) -> bool {
    INSTALLED_INTEGRATIONS_IGNORED_PLUGINS.contains(&plugin_id)
}

/// Port of `model.InstalledIntegration` (usage.go:26).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InstalledIntegration {
    /// `"plugin"` or `"app"`.
    #[serde(rename = "type")]
    pub type_: String,

    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "version")]
    pub version: String,

    #[serde(rename = "enabled")]
    pub enabled: bool,
}

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
    fn posts_usage_round_trips_the_fixture() {
        assert_fixture_round_trips!(PostsUsage, "posts_usage");
    }
    #[test]
    fn storage_usage_round_trips_the_fixture() {
        assert_fixture_round_trips!(StorageUsage, "storage_usage");
    }
    #[test]
    fn teams_usage_round_trips_the_fixture() {
        assert_fixture_round_trips!(TeamsUsage, "teams_usage");
    }
    #[test]
    fn installed_integration_round_trips_the_fixture() {
        assert_fixture_round_trips!(InstalledIntegration, "installed_integration");
    }
}
