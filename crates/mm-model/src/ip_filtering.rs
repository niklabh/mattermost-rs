//! Port of `model/ip_filtering.go` — the Cloud IP allow-list.

use serde::{Deserialize, Serialize};

/// Port of `model.AllowedIPRange` (ip_filtering.go:5).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AllowedIPRange {
    /// CIDR notation, e.g. `10.0.0.0/8`. **Nothing in this file parses or validates it** — the
    /// check happens in the Cloud service.
    #[serde(rename = "cidr_block")]
    pub cidr_block: String,

    #[serde(rename = "description")]
    pub description: String,

    #[serde(rename = "enabled")]
    pub enabled: bool,

    #[serde(rename = "owner_id")]
    pub owner_id: String,
}

/// Port of `model.AllowedIPRanges` (ip_filtering.go:3) — a `[]AllowedIPRange`, so `null` when nil.
///
/// Its `Auditable` wraps the whole slice under the key `AllowedIPRanges`; auditing is [D-028] and
/// is not ported here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AllowedIPRanges(pub Vec<AllowedIPRange>);

/// Port of `model.GetIPAddressResponse` (ip_filtering.go:19).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GetIPAddressResponse {
    #[serde(rename = "ip")]
    pub ip: String,
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
    fn allowed_ip_range_round_trips_the_fixture() {
        assert_fixture_round_trips!(AllowedIPRange, "allowed_ip_range");
    }
    #[test]
    fn get_ip_address_response_round_trips_the_fixture() {
        assert_fixture_round_trips!(GetIPAddressResponse, "get_ip_address_response");
    }
}
