//! Port of `model/cel.go` — the "visual expression" form of an access-control policy.
//!
//! `Condition.Value` is Go's bare `any`: a single value for `==`, a list for `in`. Per CLAUDE.md
//! that is [`serde_json::Value`], not a typed enum — the Go source does not prove the shape.

use serde::{Deserialize, Serialize};

fn is_false(b: &bool) -> bool {
    !*b
}

/// Port of `model.ValueType` (cel.go:7) — a Go `int` with `iota` constants, so it is a **number**
/// on the wire, not a string.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ValueType(pub i64);

impl ValueType {
    /// `LiteralValue` (cel.go:10) — iota, so zero, so also the `Default`.
    pub const LITERAL: ValueType = ValueType(0);
    /// `AttrValue` (cel.go:11).
    pub const ATTR: ValueType = ValueType(1);
}

/// Port of `model.Condition` (cel.go:15).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Condition {
    /// Left-hand attribute selector, e.g. `user.attributes.Team`.
    #[serde(rename = "attribute")]
    pub attribute: String,

    #[serde(rename = "operator")]
    pub operator: String,

    /// A single value, or a list for `in`. Go's `any`.
    #[serde(rename = "value")]
    pub value: serde_json::Value,

    /// Which of the two sides `value` is — needed for `user.attr1 == user.attr2`.
    #[serde(rename = "value_type")]
    pub value_type: ValueType,

    /// `text`, `select`, `multiselect`, …
    #[serde(rename = "attribute_type")]
    pub attribute_type: String,

    /// Set when values the caller may not see were dropped from this condition.
    #[serde(rename = "has_masked_values", skip_serializing_if = "is_false")]
    pub has_masked_values: bool,
}

/// Port of `model.VisualExpression` (cel.go:31) — conditions ANDed together.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VisualExpression {
    #[serde(rename = "conditions")]
    pub conditions: Vec<Condition>,
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
    fn condition_round_trips_the_fixture() {
        assert_fixture_round_trips!(Condition, "condition");
    }
    #[test]
    fn visual_expression_round_trips_the_fixture() {
        assert_fixture_round_trips!(VisualExpression, "visual_expression");
    }
}
