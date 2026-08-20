//! Port of `model/read_receipt.go` — whole file, three fields and no methods.

use serde::{Deserialize, Serialize};

/// Port of `model.ReadReceipt` (read_receipt.go:6).
///
/// `expire_at` is Go's epoch **milliseconds** as `int64`, like every other timestamp in the tree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReadReceipt {
    #[serde(rename = "post_id")]
    pub post_id: String,

    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "expire_at")]
    pub expire_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialization_parity_with_the_fixture() {
        let raw = include_str!("../../../fixtures/read_receipt.json");
        let receipt: ReadReceipt = serde_json::from_str(raw).unwrap();
        let expected: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_value(&receipt).unwrap(), expected);
    }

    #[test]
    fn the_zero_value_is_three_keys_in_gos_order() {
        assert_eq!(
            serde_json::to_string(&ReadReceipt::default()).unwrap(),
            r#"{"post_id":"","user_id":"","expire_at":0}"#
        );
    }
}
