//! Port of `model/push_response.go` — whole file: five constants, a map newtype and three
//! constructors.
//!
//! # The constant names and the wire keys disagree
//!
//! `PushStatusErrorMsg` is `"error"`, not `"error_msg"`, and `PushStatus` is `"status"` — so an
//! error response is `{"error":"…","status":"FAIL"}`. Nothing in the Go source spells that out;
//! it falls out of two constants whose names read like the other value. Every one is pinned
//! against the generated corpus rather than transcribed.
//!
//! # A newtype, not an alias
//!
//! Go's `type PushResponse map[string]string` carries methods, so an alias would let any
//! `StringMap` stand in for one. `#[serde(transparent)]` keeps the wire form a bare object.

use serde::{Deserialize, Serialize};

use crate::utils::StringMap;

/// `model.PushStatus` (push_response.go:7) — the key, not a value.
pub const PUSH_STATUS: &str = "status";
/// `model.PushStatusOk` (push_response.go:8).
pub const PUSH_STATUS_OK: &str = "OK";
/// `model.PushStatusFail` (push_response.go:9).
pub const PUSH_STATUS_FAIL: &str = "FAIL";
/// `model.PushStatusRemove` (push_response.go:10).
pub const PUSH_STATUS_REMOVE: &str = "REMOVE";
/// `model.PushStatusErrorMsg` (push_response.go:11) — the key is **`error`**, not `error_msg`.
pub const PUSH_STATUS_ERROR_MSG: &str = "error";

/// Port of `model.PushResponse` (push_response.go:14).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PushResponse(pub StringMap);

impl PushResponse {
    /// Port of `model.NewOkPushResponse` (push_response.go:16).
    pub fn ok() -> Self {
        Self(StringMap::from([(
            PUSH_STATUS.to_owned(),
            PUSH_STATUS_OK.to_owned(),
        )]))
    }

    /// Port of `model.NewRemovePushResponse` (push_response.go:22).
    pub fn remove() -> Self {
        Self(StringMap::from([(
            PUSH_STATUS.to_owned(),
            PUSH_STATUS_REMOVE.to_owned(),
        )]))
    }

    /// Port of `model.NewErrorPushResponse` (push_response.go:28).
    ///
    /// The message is written unconditionally — Go does not branch on it, so an empty message
    /// still produces the `error` key rather than omitting it.
    pub fn error(message: &str) -> Self {
        Self(StringMap::from([
            (PUSH_STATUS.to_owned(), PUSH_STATUS_FAIL.to_owned()),
            (PUSH_STATUS_ERROR_MSG.to_owned(), message.to_owned()),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_small_types.json")).unwrap()
    }

    /// The five constants, read out of Go rather than transcribed from the source.
    #[test]
    fn go_parity_the_constants() {
        let c = &corpus()["push_constants"];
        assert_eq!(c["PushStatus"].as_str(), Some(PUSH_STATUS));
        assert_eq!(c["PushStatusOk"].as_str(), Some(PUSH_STATUS_OK));
        assert_eq!(c["PushStatusFail"].as_str(), Some(PUSH_STATUS_FAIL));
        assert_eq!(c["PushStatusRemove"].as_str(), Some(PUSH_STATUS_REMOVE));
        assert_eq!(
            c["PushStatusErrorMsg"].as_str(),
            Some(PUSH_STATUS_ERROR_MSG),
            "the constant is named ErrorMsg and its value is `error`"
        );
    }

    /// All three constructors, byte for byte against Go's own marshalling.
    #[test]
    fn go_parity_the_constructors() {
        let corpus = corpus();
        let rows = corpus["push_responses"].as_array().unwrap();

        let ours = |name: &str| -> PushResponse {
            match name {
                "ok" => PushResponse::ok(),
                "remove" => PushResponse::remove(),
                "error" => PushResponse::error("boom"),
                "error_empty" => PushResponse::error(""),
                other => panic!("unknown corpus row {other}"),
            }
        };

        assert_eq!(rows.len(), 4);
        for row in rows {
            let name = row["name"].as_str().unwrap();
            let expected: serde_json::Value =
                serde_json::from_str(row["out"].as_str().unwrap()).unwrap();
            assert_eq!(
                serde_json::to_value(ours(name)).unwrap(),
                expected,
                "{name}"
            );
        }
    }

    /// An empty message keeps the key. Asserted separately because it is the one branch a reader
    /// would add.
    #[test]
    fn an_empty_error_message_still_writes_the_key() {
        let response = PushResponse::error("");
        assert_eq!(response.0.get(PUSH_STATUS_ERROR_MSG), Some(&String::new()));
        assert_eq!(response.0.len(), 2);
    }

    /// An empty response is `{}`, not `null` — the newtype wraps a map, and `transparent` keeps
    /// the object shape.
    #[test]
    fn the_empty_response_is_an_empty_object() {
        assert_eq!(
            serde_json::to_string(&PushResponse::default()).unwrap(),
            "{}"
        );
        assert_eq!(
            corpus()["zero_values"]["push_response"].as_str(),
            Some("{}")
        );
    }
}
