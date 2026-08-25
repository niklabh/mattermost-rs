//! Port of `model/onboarding.go` — the body of the complete-onboarding route.

use serde::{Deserialize, Serialize};

/// Port of `model.CompleteOnboardingRequest` (onboarding.go:9).
///
/// `Auditable` records only `install_plugins`: the organisation name is customer data and is
/// deliberately kept out of the audit log.
///
/// `CompleteOnboardingRequestFromReader` is not ported — it is `json.NewDecoder(...).Decode`,
/// which `serde_json::from_reader`/`from_slice` already is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompleteOnboardingRequest {
    #[serde(rename = "organization")]
    pub organization: String,

    #[serde(rename = "install_plugins")]
    pub install_plugins: Vec<String>,
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
    fn complete_onboarding_request_round_trips_the_fixture() {
        assert_fixture_round_trips!(CompleteOnboardingRequest, "complete_onboarding_request");
    }
}
