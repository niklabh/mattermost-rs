//! Port of `model/channel_member_history_result.go` — the join of ChannelMemberHistory onto Users.
//!
//! No `json:` tags anywhere: this is a store row, not a wire type. The one `db:` tag matters —
//! `UserEmail` is selected as **`Email`**, so a query that aliases it as `user_email` returns a
//! row this type cannot fill.

/// Port of `model.ChannelMemberHistoryResult` (channel_member_history_result.go:6).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelMemberHistoryResult {
    pub channel_id: String,
    pub user_id: String,
    pub join_time: i64,
    /// `*int64` — still in the channel when nil.
    pub leave_time: Option<i64>,

    /// Never stored on ChannelMemberHistory; joined in from Users. `db:"Email"`.
    pub user_email: String,
    pub username: String,
    pub is_bot: bool,
    pub user_delete_at: i64,
}
