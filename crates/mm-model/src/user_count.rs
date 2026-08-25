//! Port of `model/user_count.go` — the option bag for counting users.
//!
//! No `json:` tags: this is assembled server-side from query parameters and the caller's
//! permissions, never decoded from a body.

use crate::user::ViewUsersRestrictions;

/// Port of `model.UserCountOptions` (user_count.go:4).
///
/// `ExcludeRegularUsers` reads like the inverse of `IncludeBotAccounts` but is not: with both
/// set, the count is bots only; with neither, it is regular users only. They are independent
/// filters over the same population.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserCountOptions {
    pub include_bot_accounts: bool,
    /// Deleted users of **any** type.
    pub include_deleted: bool,
    pub include_remote_users: bool,
    pub exclude_regular_users: bool,
    /// `""` for any team.
    pub team_id: String,
    /// `""` for any channel.
    pub channel_id: String,
    pub view_restrictions: Option<ViewUsersRestrictions>,
    /// System-wide roles; a user matching **any** of them is counted.
    pub roles: Vec<String>,
    /// Requires `channel_id`.
    pub channel_roles: Vec<String>,
    /// Requires `team_id`.
    pub team_roles: Vec<String>,
}
