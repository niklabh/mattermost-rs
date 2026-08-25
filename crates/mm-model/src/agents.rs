//! Port of `model/agents.go` — three flat wire types for the AI-bridge agent listing.
//!
//! Note the casing: `BridgeAgentInfo.DisplayName` is tagged **`displayName`**, camelCase, sitting
//! between `id` and `username` and directly above two snake_case tags. That is Go's tag, not a
//! typo to tidy up.

use serde::{Deserialize, Serialize};

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_empty(s: &str) -> bool {
    s.is_empty()
}

/// Port of `model.BridgeAgentInfo` (agents.go:6).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BridgeAgentInfo {
    #[serde(rename = "id")]
    pub id: String,

    /// camelCase in Go — `displayName`, not `display_name`.
    #[serde(rename = "displayName")]
    pub display_name: String,

    #[serde(rename = "username")]
    pub username: String,

    #[serde(rename = "service_id")]
    pub service_id: String,

    #[serde(rename = "service_type")]
    pub service_type: String,

    #[serde(rename = "is_default", skip_serializing_if = "is_false")]
    pub is_default: bool,
}

/// Port of `model.BridgeServiceInfo` (agents.go:15).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BridgeServiceInfo {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "type")]
    pub type_: String,
}

/// Port of `model.AgentsIntegrityResponse` (agents.go:21).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentsIntegrityResponse {
    #[serde(rename = "available")]
    pub available: bool,

    #[serde(rename = "reason", skip_serializing_if = "is_empty")]
    pub reason: String,
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
    fn bridge_agent_info_round_trips_the_fixture() {
        assert_fixture_round_trips!(BridgeAgentInfo, "bridge_agent_info");
    }
    #[test]
    fn bridge_service_info_round_trips_the_fixture() {
        assert_fixture_round_trips!(BridgeServiceInfo, "bridge_service_info");
    }
    #[test]
    fn agents_integrity_response_round_trips_the_fixture() {
        assert_fixture_round_trips!(AgentsIntegrityResponse, "agents_integrity_response");
    }
}
