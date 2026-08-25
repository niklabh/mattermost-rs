//! Port of `model/user_search.go` — the search request and the options derived from it.
//!
//! [`UserSearch`] is the client's body and is fully tagged; [`UserSearchOptions`] is what the
//! server builds from it *plus the caller's permissions* and has no tags at all. The split is the
//! security boundary: `AllowEmails`, `AllowFullNames` and `AllowInactive` are decisions the
//! server makes, and a client that could set them would be reading colleagues' email addresses.

use serde::{Deserialize, Serialize};

use crate::user::ViewUsersRestrictions;

/// Port of `model.UserSearchMaxLimit` (user_search.go:3).
pub const USER_SEARCH_MAX_LIMIT: i64 = 1000;
/// Port of `model.UserSearchDefaultLimit` (user_search.go:4).
pub const USER_SEARCH_DEFAULT_LIMIT: i64 = 100;

/// Port of `model.UserSearch` (user_search.go:7).
///
/// **`not_in_group_id` is declared last**, after the three role slices, rather than beside
/// `in_group_id`. Go marshals in declaration order, so that is where the key appears.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserSearch {
    #[serde(rename = "term")]
    pub term: String,

    #[serde(rename = "team_id")]
    pub team_id: String,

    #[serde(rename = "not_in_team_id")]
    pub not_in_team_id: String,

    #[serde(rename = "in_channel_id")]
    pub in_channel_id: String,

    #[serde(rename = "not_in_channel_id")]
    pub not_in_channel_id: String,

    #[serde(rename = "in_group_id")]
    pub in_group_id: String,

    #[serde(rename = "group_constrained")]
    pub group_constrained: bool,

    /// A *request*, not a grant — the server still decides via
    /// [`UserSearchOptions::allow_inactive`].
    #[serde(rename = "allow_inactive")]
    pub allow_inactive: bool,

    #[serde(rename = "without_team")]
    pub without_team: bool,

    /// Capped at [`USER_SEARCH_MAX_LIMIT`] by the caller, not here.
    #[serde(rename = "limit")]
    pub limit: i64,

    #[serde(rename = "role")]
    pub role: String,

    #[serde(rename = "roles")]
    pub roles: Option<Vec<String>>,

    #[serde(rename = "channel_roles")]
    pub channel_roles: Option<Vec<String>>,

    #[serde(rename = "team_roles")]
    pub team_roles: Option<Vec<String>>,

    #[serde(rename = "not_in_group_id")]
    pub not_in_group_id: String,
}

/// Port of `model.UserSearchOptions` (user_search.go:26). Server-side only — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserSearchOptions {
    pub is_admin: bool,
    /// Lets the search match on email addresses.
    pub allow_emails: bool,
    /// Lets the search match on full names, rather than usernames and nicknames only.
    pub allow_full_names: bool,
    pub allow_inactive: bool,
    pub group_constrained: bool,
    pub limit: i64,
    pub role: String,
    pub roles: Vec<String>,
    pub channel_roles: Vec<String>,
    pub team_roles: Vec<String>,
    pub view_restrictions: Option<ViewUsersRestrictions>,
    pub list_of_allowed_channels: Vec<String>,
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
    fn user_search_round_trips_the_fixture() {
        assert_fixture_round_trips!(UserSearch, "user_search");
    }
}
