//! Port of `model/group_member.go` — membership of a user in a custom or LDAP-synced group.

use serde::{Deserialize, Serialize};

use crate::user::User;
use crate::utils::{AppError, AppResult, is_valid_id};

/// Port of `model.GroupMember` (group_member.go:5).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupMember {
    #[serde(rename = "group_id")]
    pub group_id: String,

    #[serde(rename = "user_id")]
    pub user_id: String,

    /// Epoch milliseconds.
    #[serde(rename = "create_at")]
    pub create_at: i64,

    /// Epoch milliseconds; membership is soft-deleted.
    #[serde(rename = "delete_at")]
    pub delete_at: i64,
}

impl GroupMember {
    /// Port of `(*GroupMember).IsValid` (group_member.go:12).
    ///
    /// The error ids are `model.group_member.<field>.app_error` — **no `is_valid` segment**.
    /// Neither timestamp is checked.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.group_id) {
            return Err(err("group_id"));
        }
        if !is_valid_id(&self.user_id) {
            return Err(err("user_id"));
        }
        Ok(())
    }
}

fn err(field: &str) -> Box<AppError> {
    Box::new(AppError::new(
        "GroupMember.IsValid",
        format!("model.group_member.{field}.app_error"),
        None,
        "",
        400,
    ))
}

/// Port of `model.GroupMemberList` (group_member.go:22).
///
/// The count is tagged **`total_member_count`** while the field is `Count` — it is the total
/// across all pages, not `members.len()`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupMemberList {
    #[serde(rename = "members")]
    pub members: Option<Vec<User>>,

    #[serde(rename = "total_member_count")]
    pub count: i64,
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
    fn group_member_round_trips_the_fixture() {
        assert_fixture_round_trips!(GroupMember, "group_member");
    }
    #[test]
    fn group_member_list_round_trips_the_fixture() {
        assert_fixture_round_trips!(GroupMemberList, "group_member_list");
    }
}
