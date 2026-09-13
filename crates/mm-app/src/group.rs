//! Port of the one read from `channels/app/group.go` a route this server answers needs.
//!
//! # Why this file exists at all
//!
//! Every one of the twenty routes in `api4/group.go` opens with `requireLicense` and is answered
//! here as a 501 (see `mm_api::groups`), so nothing in that file reaches a group table. The
//! exception is `channelMembersMinusGroupMembers` (api4/channel.go:2881), which lives in the
//! *channel* file, has **no licence gate**, and answers a group question on an unlicensed server.
//! It is the single caller of everything below.

use mm_model::group::Group;
use mm_model::user::UserWithGroups;
use mm_model::utils::{AppError, AppResult};
use mm_store::group_store::GroupStore;

use crate::App;

impl App {
    /// Port of `App.ChannelMembersMinusGroupMembers` (app/group.go:755).
    ///
    /// The members of `channel_id` who are in **none** of `group_ids` — "who would be removed if
    /// this channel were group-constrained to these groups" — one page of them, plus the total.
    ///
    /// # The group hydration is a second query, and an id it does not resolve is dropped
    ///
    /// The page query returns each user's group ids as one `string_agg` column; this then fetches
    /// the distinct set in one `GetByIDs` and maps them back. Go's inner loop is
    /// `if group, ok := groupMap[groupID]; ok` — so a group id that `GetByIDs` did not return
    /// (which cannot happen through a foreign key, but the code allows it) is **skipped**, not
    /// nil-appended. Reproduced with `filter_map`.
    ///
    /// # `Groups` is always a list, never null
    ///
    /// `user.Groups = []*model.Group{}` runs before the inner loop for every user
    /// (app/group.go:791), so a member of no group is `"groups": []` on the wire and the store's
    /// nil is never what a client sees. That assignment is the whole reason `UserWithGroups.groups`
    /// is an `Option` at all — the type can express Go's nil, and this function is where it stops
    /// being one.
    ///
    /// # `SanitizeProfile(&u.User, false)` — `false`, not `true`
    ///
    /// This is a System Console route reached with `sysconsole_read_user_management_channels`, and
    /// it still sanitises as a **non-admin**: `ShowEmailAddress` and `ShowFullName` decide whether
    /// the email and the names survive, exactly as they do for any other caller. A port that
    /// passed `true` would leak both on a privacy-configured server.
    #[tracing::instrument(skip(self, group_ids), fields(channel_id = %channel_id, groups = group_ids.len(), users, total))]
    pub async fn channel_members_minus_group_members(
        &self,
        channel_id: &str,
        group_ids: &[String],
        page: i64,
        per_page: i64,
    ) -> AppResult<(Vec<UserWithGroups>, i64)> {
        let mut users = self
            .store
            .group()
            .channel_members_minus_group_members(channel_id, group_ids, page, per_page)
            .await
            .map_err(|source| {
                AppError::boxed(
                    "ChannelMembersMinusGroupMembers",
                    "app.select_error",
                    None,
                    source.to_string(),
                    500,
                )
            })?;

        for user in &mut users {
            self.sanitize_profile(&mut user.user, false);
        }

        // The distinct group ids across every user on this page, in Go's `map`-then-slice shape.
        // Order does not reach the wire — the per-user loop below re-orders by each user's own
        // `GetGroupIDs` — so a `BTreeSet` buys determinism for free.
        let wanted: Vec<String> = users
            .iter()
            .filter_map(UserWithGroups::get_group_ids)
            .flatten()
            .map(str::to_owned)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();

        let groups = self.get_groups_by_ids(&wanted).await?;
        let by_id: std::collections::HashMap<&str, &Group> =
            groups.iter().map(|g| (g.id.as_str(), g)).collect();

        for user in &mut users {
            let hydrated = user
                .get_group_ids()
                .unwrap_or_default()
                .into_iter()
                // A clone per group is the answer's own storage, not a borrow-checker dodge:
                // `groups` is local and each user's list is an independent copy, as it is in Go.
                .filter_map(|id| by_id.get(id).map(|g| (*g).clone()))
                .collect();
            user.groups = Some(hydrated);
        }

        let total = self
            .store
            .group()
            .count_channel_members_minus_group_members(channel_id, group_ids)
            .await
            .map_err(|source| {
                AppError::boxed(
                    "ChannelMembersMinusGroupMembers",
                    "app.select_error",
                    None,
                    source.to_string(),
                    500,
                )
            })?;

        tracing::Span::current().record("users", users.len());
        tracing::Span::current().record("total", total);
        Ok((users, total))
    }

    /// Port of `App.GetGroupsByIDs` (app/group.go:740).
    ///
    /// **`app.select_error` at 500**, the generic store error id, not one of its own.
    #[tracing::instrument(skip(self, group_ids), fields(asked = group_ids.len()))]
    pub async fn get_groups_by_ids(&self, group_ids: &[String]) -> AppResult<Vec<Group>> {
        self.store
            .group()
            .get_groups_by_ids(group_ids)
            .await
            .map_err(|source| {
                AppError::boxed(
                    "GetGroupsByIDs",
                    "app.select_error",
                    None,
                    source.to_string(),
                    500,
                )
            })
    }
}
