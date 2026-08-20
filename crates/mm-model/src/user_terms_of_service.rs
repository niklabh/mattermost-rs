//! Port of `model/user_terms_of_service.go` — the wire struct only.
//!
//! `IsValid` and `PreSave` belong to the save path (`saveUserTermsOfService`), which is not
//! served; they land with that route. What `getUser` needs is the three-field read model whose
//! `terms_of_service_id` / `create_at` are copied onto the `User` body for the self-or-admin
//! viewer.

use serde::{Deserialize, Serialize};

/// Port of `model.UserTermsOfService` (user_terms_of_service.go:11).
///
/// No `omitempty` anywhere: the zero value serialises all three keys.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserTermsOfService {
    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "terms_of_service_id")]
    pub terms_of_service_id: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round trip through the generated fixture: decode, re-encode, compare value graphs.
    #[test]
    fn serialization_parity_with_the_fixture() {
        let raw = include_str!("../../../fixtures/user_terms_of_service.json");
        let parsed: UserTermsOfService = serde_json::from_str(raw).unwrap();
        let expected: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), expected);
        // The fixture is only evidence if nothing in it is a zero value.
        assert!(!parsed.user_id.is_empty());
        assert!(!parsed.terms_of_service_id.is_empty());
        assert_ne!(parsed.create_at, 0);
    }

    /// No `omitempty`: the zero value is three keys, in Go's declaration order.
    #[test]
    fn the_zero_value_keeps_all_three_keys() {
        assert_eq!(
            serde_json::to_string(&UserTermsOfService::default()).unwrap(),
            r#"{"user_id":"","terms_of_service_id":"","create_at":0}"#
        );
    }
}
