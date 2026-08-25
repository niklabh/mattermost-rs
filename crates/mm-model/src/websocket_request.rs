//! Port of `model/websocket_request.go` — a request arriving over a websocket.

use serde::{Deserialize, Serialize};

use crate::session::Session;
use crate::utils::StringInterface;

/// Port of `model.WebSocketRemoteAddr` (websocket_request.go:12) — a `Data` key.
pub const WEBSOCKET_REMOTE_ADDR: &str = "remote_addr";
/// Port of `model.WebSocketXForwardedFor` (websocket_request.go:13).
pub const WEBSOCKET_X_FORWARDED_FOR: &str = "x_forwarded_for";

/// Port of `model.WebSocketRequest` (websocket_request.go:17).
///
/// The three server-provided fields carry `json:"-"` **and** `msgpack:"-"`: the client cannot set
/// them, and `Clone` — which round-trips through msgpack — **drops them**. That is Go's behaviour
/// and it is why `Clone` is not a copy: a cloned request has a zero `Session` and an empty
/// locale.
///
/// The msgpack tags mirror the JSON ones exactly, so a client may speak either encoding; this
/// port carries the JSON side, and the tag names are identical either way.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSocketRequest {
    /// Incremented by the client for every request; the response echoes it.
    #[serde(rename = "seq")]
    pub seq: i64,

    /// `get_statuses`, `user_typing`, …
    #[serde(rename = "action")]
    pub action: String,

    #[serde(rename = "data")]
    pub data: Option<StringInterface>,

    /// `json:"-"` — filled by the server from the connection.
    #[serde(skip)]
    pub session: Session,

    /// `json:"-"`. Go also carries a `T` translate function here; i18n is a server concern and
    /// a function is not data, so it has no counterpart.
    #[serde(skip)]
    pub locale: String,
}

impl WebSocketRequest {
    /// Port of `(*WebSocketRequest).Clone` (websocket_request.go:30).
    ///
    /// **Not a deep copy of the whole struct**: Go round-trips through msgpack, which honours the
    /// `msgpack:"-"` tags, so the session and locale are lost. Reproduced by clearing them.
    ///
    /// Go's signature returns an error because marshalling can fail; here it cannot.
    pub fn clone_for_dispatch(&self) -> WebSocketRequest {
        WebSocketRequest {
            seq: self.seq,
            action: self.action.clone(),
            data: self.data.clone(),
            session: Session::default(),
            locale: String::new(),
        }
    }
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
    fn web_socket_request_round_trips_the_fixture() {
        assert_fixture_round_trips!(WebSocketRequest, "web_socket_request");
    }
}
