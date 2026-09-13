//! Port of the two reads from `channels/app/group.go` the routes this server answers need.
//!
//! # Why this file exists at all
//!
//! Every one of the twenty routes in `api4/group.go` opens with `requireLicense` and is answered
//! here as a 501 (see `mm_api::groups`), so nothing in that file reaches a group table. The
//! exceptions are `channelMembersMinusGroupMembers` (api4/channel.go:2881) and
//! `teamMembersMinusGroupMembers` (api4/team.go:2222), which live in the *channel* and *team*
//! files, have **no licence gate**, and answer a group question on an unlicensed server. They are
//! the only callers of everything below.

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

        self.sanitize_and_hydrate_groups(&mut users).await?;

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

    /// Port of `App.TeamMembersMinusGroupMembers` (app/group.go:687).
    ///
    /// The members of `team_id` who are in **none** of `group_ids`, one page of them, plus the
    /// total. Character-for-character the same function as
    /// [`App::channel_members_minus_group_members`] with the store pair swapped — including the
    /// `SanitizeProfile(..., false)`, the `Groups = []` assignment and the dropped-id `filter_map`,
    /// all of which are documented there. Go duplicates the body too; the shared half lives in
    /// [`App::sanitize_and_hydrate_groups`].
    ///
    /// **The `where` on a store failure is `TeamMembersMinusGroupMembers`**, not the channel
    /// name — the only thing a client can tell apart when the database is down.
    #[tracing::instrument(skip(self, group_ids), fields(team_id = %team_id, groups = group_ids.len(), users, total))]
    pub async fn team_members_minus_group_members(
        &self,
        team_id: &str,
        group_ids: &[String],
        page: i64,
        per_page: i64,
    ) -> AppResult<(Vec<UserWithGroups>, i64)> {
        let mut users = self
            .store
            .group()
            .team_members_minus_group_members(team_id, group_ids, page, per_page)
            .await
            .map_err(|source| {
                AppError::boxed(
                    "TeamMembersMinusGroupMembers",
                    "app.select_error",
                    None,
                    source.to_string(),
                    500,
                )
            })?;

        self.sanitize_and_hydrate_groups(&mut users).await?;

        let total = self
            .store
            .group()
            .count_team_members_minus_group_members(team_id, group_ids)
            .await
            .map_err(|source| {
                AppError::boxed(
                    "TeamMembersMinusGroupMembers",
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

    /// The half of `TeamMembersMinusGroupMembers` and `ChannelMembersMinusGroupMembers` that Go
    /// writes out twice: sanitise every profile as a non-admin, then replace each user's
    /// `string_agg`ed group ids with the group rows themselves.
    ///
    /// Every decision here is documented on [`App::channel_members_minus_group_members`]; this is
    /// where they are actually taken.
    async fn sanitize_and_hydrate_groups(&self, users: &mut [UserWithGroups]) -> AppResult {
        for user in users.iter_mut() {
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

        for user in users.iter_mut() {
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

        Ok(())
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
