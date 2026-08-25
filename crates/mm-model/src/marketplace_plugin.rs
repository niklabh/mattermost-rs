//! Port of `model/marketplace_plugin.go` — plugins as described by the Marketplace server.

use serde::{Deserialize, Serialize};

use crate::go_url::Values;
use crate::manifest::Manifest;
use crate::serde_helpers::is_none_or_empty_vec;

/// Port of `model.BaseMarketplacePlugin` (marketplace_plugin.go:16) — the Marketplace's own
/// record.
///
/// `Signature` is a base64 string covering the plugin bundle; `IconData` is a data URI. Only
/// `labels` carries `omitempty`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BaseMarketplacePlugin {
    #[serde(rename = "homepage_url")]
    pub homepage_url: String,

    /// A `data:` URI, not a path.
    #[serde(rename = "icon_data")]
    pub icon_data: String,

    #[serde(rename = "download_url")]
    pub download_url: String,

    #[serde(rename = "release_notes_url")]
    pub release_notes_url: String,

    #[serde(rename = "labels", skip_serializing_if = "is_none_or_empty_vec")]
    pub labels: Option<Vec<MarketplaceLabel>>,

    /// Restricts the plugin to a hosting type (`cloud` / `on-prem`), or empty for both.
    #[serde(rename = "hosting")]
    pub hosting: String,

    #[serde(rename = "author_type")]
    pub author_type: String,

    /// Where in the release cycle the plugin is.
    #[serde(rename = "release_stage")]
    pub release_stage: String,

    #[serde(rename = "enterprise")]
    pub enterprise: bool,

    /// base64.
    #[serde(rename = "signature")]
    pub signature: String,

    #[serde(rename = "manifest")]
    pub manifest: Option<Manifest>,
}

impl BaseMarketplacePlugin {
    /// Port of `(*BaseMarketplacePlugin).DecodeSignature` (marketplace_plugin.go:71).
    ///
    /// Go returns an `io.ReadSeeker` over the decoded bytes; a `Vec<u8>` is the same thing here,
    /// and the caller wraps it in a `Cursor` if it needs to seek.
    pub fn decode_signature(&self) -> Result<Vec<u8>, MarketplaceError> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(self.signature.as_bytes())
            .map_err(|_| MarketplaceError::UndecodableSignature)
    }
}

/// Port of `model.MarketplaceLabel` (marketplace_plugin.go:31) — a badge in the Marketplace UI.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MarketplaceLabel {
    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "description")]
    pub description: String,

    #[serde(rename = "url")]
    pub url: String,

    #[serde(rename = "color")]
    pub color: String,
}

/// Port of `model.MarketplacePlugin` (marketplace_plugin.go:39) — the Marketplace record plus
/// this server's state.
///
/// The embedded pointer is **inlined** by `encoding/json`, so `installed_version` sits beside
/// `download_url` at the top level.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MarketplacePlugin {
    #[serde(flatten)]
    pub base: BaseMarketplacePlugin,

    /// Empty when the plugin is not installed here.
    #[serde(rename = "installed_version")]
    pub installed_version: String,
}

/// Port of `model.MarketplacePluginFilter` (marketplace_plugin.go:80) — the query the server
/// sends to the Marketplace. No `json:` tags; it becomes a query string.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarketplacePluginFilter {
    pub page: i64,
    pub per_page: i64,
    pub filter: String,
    pub server_version: String,
    pub build_enterprise_ready: bool,
    pub enterprise_plugins: bool,
    pub cloud: bool,
    pub local_only: bool,
    pub platform: String,
    pub plugin_id: String,
    pub return_all_versions: bool,
    pub remote_only: bool,
}

impl MarketplacePluginFilter {
    /// Port of `(*MarketplacePluginFilter).ToValues` (marketplace_plugin.go:100).
    ///
    /// **`per_page` is the only conditional parameter** — it is omitted when not positive, while
    /// `page` is always sent, even as `0`. The booleans are rendered by `strconv.FormatBool`, so
    /// `true`/`false`, never `1`/`0`.
    pub fn to_values(&self) -> Values {
        let mut q = Values::new();
        q.set("page", &self.page.to_string());
        if self.per_page > 0 {
            q.set("per_page", &self.per_page.to_string());
        }
        q.set("filter", &self.filter);
        q.set("server_version", &self.server_version);
        q.set(
            "build_enterprise_ready",
            bool_str(self.build_enterprise_ready),
        );
        q.set("enterprise_plugins", bool_str(self.enterprise_plugins));
        q.set("cloud", bool_str(self.cloud));
        q.set("local_only", bool_str(self.local_only));
        q.set("remote_only", bool_str(self.remote_only));
        q.set("platform", &self.platform);
        q.set("plugin_id", &self.plugin_id);
        q.set("return_all_versions", bool_str(self.return_all_versions));
        q
    }

    /// Port of `(*MarketplacePluginFilter).ApplyToURL` (marketplace_plugin.go:95) — replaces the
    /// URL's raw query wholesale.
    pub fn apply_to_url(&self, u: &mut crate::go_url::GoUrl) {
        u.raw_query = self.to_values().encode();
    }
}

fn bool_str(b: bool) -> &'static str {
    if b { "true" } else { "false" }
}

/// Port of `model.InstallMarketplacePluginRequest` (marketplace_plugin.go:118).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InstallMarketplacePluginRequest {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "version")]
    pub version: String,
}

/// The one error this file returns. The `*FromReader` helpers are not ported —
/// `serde_json::from_slice` over a list is the same thing, and Go's "treat `io.EOF` as an empty
/// list" special case is an artifact of streaming decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MarketplaceError {
    #[error("Unable to decode base64 signature.")]
    UndecodableSignature,
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
    fn base_marketplace_plugin_round_trips_the_fixture() {
        assert_fixture_round_trips!(BaseMarketplacePlugin, "base_marketplace_plugin");
    }
    #[test]
    fn marketplace_label_round_trips_the_fixture() {
        assert_fixture_round_trips!(MarketplaceLabel, "marketplace_label");
    }
    #[test]
    fn marketplace_plugin_round_trips_the_fixture() {
        assert_fixture_round_trips!(MarketplacePlugin, "marketplace_plugin");
    }
    #[test]
    fn install_marketplace_plugin_request_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            InstallMarketplacePluginRequest,
            "install_marketplace_plugin_request"
        );
    }
}
