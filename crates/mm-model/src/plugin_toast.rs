//! Port of `model/plugin_toast.go` — options for a toast pushed to one user.

use serde::{Deserialize, Serialize};

use crate::serde_helpers::is_empty_str;

/// Port of `model.SendToastMessageOptions` (plugin_toast.go:6).
///
/// The valid positions are `top-left`, `top-center`, `top-right`, `bottom-left`,
/// `bottom-center`, `bottom-right`. **Nothing on the server validates this** — an empty or
/// unrecognised value falls back to `bottom-right` in the web app, so a typo is silent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SendToastMessageOptions {
    #[serde(rename = "position", skip_serializing_if = "is_empty_str")]
    pub position: String,
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
    fn send_toast_message_options_round_trips_the_fixture() {
        assert_fixture_round_trips!(SendToastMessageOptions, "send_toast_message_options");
    }
}
