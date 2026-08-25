//! Port of `model/user_get.go` — the option bag for listing users.
//!
//! No `json:` tags; see `user_count.rs`.

use crate::user::ViewUsersRestrictions;

/// Port of `model.UserGetOptions` (user_get.go:3).
///
/// `Inactive` and `Active` are **separate booleans**, not one tri-state: setting both is
/// expressible and means whatever the store's WHERE clause does with two contradictory filters.
/// `Role` and `Roles` are likewise both present and both applied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserGetOptions {
    pub in_team_id: String,
    pub not_in_team_id: String,
    pub in_channel_id: String,
    pub not_in_channel_id: String,
    pub in_group_id: String,
    pub not_in_group_id: String,
    pub group_constrained: bool,
    pub without_team: bool,
    pub inactive: bool,
    pub active: bool,
    /// A single system-wide role.
    pub role: String,
    /// System-wide roles; matching **any** is enough.
    pub roles: Vec<String>,
    /// Requires `in_channel_id`.
    pub channel_roles: Vec<String>,
    /// Requires `in_team_id`.
    pub team_roles: Vec<String>,
    pub sort: String,
    pub view_restrictions: Option<ViewUsersRestrictions>,
    pub page: i64,
    pub per_page: i64,
    /// Epoch milliseconds.
    pub updated_after: i64,
}

/// Port of `model.UserGetByIdsOptions` (user_get.go:44).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UserGetByIdsOptions {
    /// Epoch milliseconds; filters on `UpdateAt`.
    pub since: i64,
}
