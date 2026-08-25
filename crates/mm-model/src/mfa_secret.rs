//! Port of `model/mfa_secret.go` — two strings, no methods.

use serde::{Deserialize, Serialize};

/// Port of `model.MfaSecret` (mfa_secret.go:3).
///
/// `qr_code` is a base64-encoded PNG **as a string**, not a `[]byte`, so it is not subject to
/// Go's `[]byte` base64 special case — it is already text by the time it lands here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MfaSecret {
    #[serde(rename = "secret")]
    pub secret: String,

    #[serde(rename = "qr_code")]
    pub qr_code: String,
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
    fn mfa_secret_round_trips_the_fixture() {
        assert_fixture_round_trips!(MfaSecret, "mfa_secret");
    }
}
