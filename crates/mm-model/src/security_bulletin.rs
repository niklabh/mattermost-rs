//! Port of `model/security_bulletin.go` — the security-notice feed the server polls.

use serde::{Deserialize, Serialize};

/// Port of `model.SecurityBulletin` (security_bulletin.go:3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityBulletin {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "applies_to_version")]
    pub applies_to_version: String,
}

/// Port of `model.SecurityBulletins` (security_bulletin.go:8) — a `[]SecurityBulletin` of
/// **values**, not pointers, unlike most named slices in the package.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecurityBulletins(pub Vec<SecurityBulletin>);

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
    fn security_bulletin_round_trips_the_fixture() {
        assert_fixture_round_trips!(SecurityBulletin, "security_bulletin");
    }
}
