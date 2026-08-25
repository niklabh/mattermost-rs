//! Port of `model/typing_request.go` — the websocket "user is typing" payload.

use serde::{Deserialize, Serialize};

/// Port of `model.TypingRequest` (typing_request.go:3).
///
/// `parent_id` is the thread root, and it keeps the pre-CRT name — not `root_id`, which is what
/// the same concept is called on `Post`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TypingRequest {
    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "parent_id")]
    pub parent_id: String,
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
    fn typing_request_round_trips_the_fixture() {
        assert_fixture_round_trips!(TypingRequest, "typing_request");
    }
}
