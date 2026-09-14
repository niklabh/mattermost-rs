//! Port of the app half of `moveChannel` (api4/channel.go) — `App.MoveChannel`
//! (app/channel.go:3822) and the two membership sweeps the handler runs before it,
//! `RemoveAllDeactivatedMembersFromChannel` (:3811) and `RemoveUsersFromChannelNotMemberOfTeam`
//! (:3953).
//!
//! # A move is five writes, and only two of them can fail the request
//!
//! In Go's order: the sidebar rows for the channel are deleted (every user's, every team's —
//! there may be no matching category in the new team), the channel row's `TeamId` is updated,
//! the channel's incoming and outgoing webhooks are re-homed, the channel's threads get the new
//! `ThreadTeamId`, and a `system_move_channel` post is written by the mover. The first two are
//! errors; the last three are `Logger().Warn` and the move succeeds without them. Then, still
//! inside the move and again only logged, every member not in the new team is removed — the
//! `force` sweep the handler may already have run, repeated after the row changed.
//!
//! # `members_do_not_match` is a **500**
//!
//! A member of the channel who is not a member of the destination team, without `force`, is
//! `app.channel.move_channel.members_do_not_match.error` at 500, not a 400 — Go's choice, kept.

use mm_model::channel::Channel;
use mm_model::post::{POST_TYPE_MOVE_CHANNEL, Post};
use mm_model::team::Team;
use mm_model::user::User;
use mm_model::utils::{AppError, AppResult};
use mm_store::channel_store::ChannelStore;
use mm_store::sidebar_category_store::SidebarCategoryStore;
use mm_store::team_store::TeamStore;
use mm_store::thread_store::ThreadStore;
use mm_store::webhook_store::WebhookStore;

use crate::App;
use crate::channel_member::MemberWrite;

/// `GetChannelMembersPage(rctx, channel.Id, 0, 10000000)` — Go's "all of them".
const ALL_MEMBERS: i64 = 10_000_000;

impl App {
    /// Port of `App.RemoveAllDeactivatedMembersFromChannel` (app/channel.go:3811): one
    /// `DELETE`, one error id.
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id))]
    pub async fn remove_all_deactivated_members_from_channel(
        &self,
        channel: &Channel,
    ) -> AppResult<()> {
        self.store()
            .channel()
            .remove_all_deactivated_members(&channel.id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "deactivated-member sweep failed");
                AppError::boxed(
                    "RemoveAllDeactivatedMembersFromChannel",
                    "app.channel.remove_all_deactivated_members.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `App.RemoveUsersFromChannelNotMemberOfTeam` (app/channel.go:3953).
    ///
    /// Every channel member without a `TeamMembers` row on `team` is removed through the
    /// **inner** `removeUserFromChannel` — the membership, the history row and the two
    /// `user_removed` events, but **no** leave or removal post. A member whose removal this port
    /// cannot reproduce (a guest, a shared channel) is a [`MemberWrite::Forward`], reported
    /// before anything of that member's is written; members earlier in the list stay removed,
    /// which is what Go's own partial failure leaves behind too.
    ///
    /// `GetTeamMembersByIds` returns only the ids that have a row — deleted memberships
    /// included, since the query does not filter `DeleteAt` — so the comparison is by count and
    /// then by set.
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id, team_id = %team.id))]
    pub async fn remove_users_from_channel_not_member_of_team(
        &self,
        remover: &User,
        channel: &Channel,
        team: &Team,
    ) -> Result<MemberWrite<()>, Box<AppError>> {
        let channel_members = self
            .get_channel_members_page(&channel.id, 0, ALL_MEMBERS)
            .await?;
        if channel_members.is_empty() {
            return Ok(MemberWrite::Done(()));
        }
        let member_ids: Vec<String> = channel_members
            .iter()
            .map(|member| member.user_id.clone())
            .collect();

        let team_members = self.get_team_members_by_ids(&team.id, &member_ids).await?;
        if team_members.len() == channel_members.len() {
            return Ok(MemberWrite::Done(()));
        }

        let in_team: std::collections::HashSet<&str> = team_members
            .iter()
            .map(|member| member.user_id.as_str())
            .collect();
        for user_id in member_ids
            .iter()
            .filter(|user_id| !in_team.contains(user_id.as_str()))
        {
            match self
                .remove_user_from_channel_inner(user_id, &remover.id, channel)
                .await?
            {
                MemberWrite::Done(_) => {}
                MemberWrite::Forward(why) => return Ok(MemberWrite::Forward(why)),
            }
        }
        Ok(MemberWrite::Done(()))
    }

    /// Port of `App.MoveChannel` (app/channel.go:3822). `channel` is updated in place — the
    /// handler encodes it, `TeamId` and the store's `UpdateAt` included.
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id, team_id = %team.id))]
    pub async fn move_channel(
        &self,
        team: &Team,
        channel: &mut Channel,
        user: &User,
    ) -> Result<MemberWrite<()>, Box<AppError>> {
        if channel.is_space() {
            return Err(AppError::boxed(
                "MoveChannel",
                "app.channel.move_channel.space.app_error",
                None,
                String::new(),
                403,
            ));
        }

        // Check that all channel members are in the destination team.
        let channel_members = self
            .get_channel_members_page(&channel.id, 0, ALL_MEMBERS)
            .await?;
        if !channel_members.is_empty() {
            let member_ids: Vec<String> = channel_members
                .iter()
                .map(|member| member.user_id.clone())
                .collect();
            let team_members = self.get_team_members_by_ids(&team.id, &member_ids).await?;
            if team_members.len() != channel_members.len() {
                let in_team: std::collections::HashSet<&str> = team_members
                    .iter()
                    .map(|member| member.user_id.as_str())
                    .collect();
                for user_id in member_ids
                    .iter()
                    .filter(|user_id| !in_team.contains(user_id.as_str()))
                {
                    tracing::warn!(user_id = %user_id, "Not member of the target team");
                }
                return Err(AppError::boxed(
                    "MoveChannel",
                    "app.channel.move_channel.members_do_not_match.error",
                    None,
                    String::new(),
                    500,
                ));
            }
        }

        // keep instance of the previous team
        let previous_team = self
            .store()
            .team()
            .get(&channel.team_id)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    AppError::boxed(
                        "MoveChannel",
                        "app.team.get.find.app_error",
                        None,
                        String::new(),
                        404,
                    )
                } else {
                    tracing::error!(error = %err, "previous team lookup failed");
                    AppError::boxed(
                        "MoveChannel",
                        "app.team.get.finding.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })?;

        self.store()
            .sidebar_category()
            .update_sidebar_channel_category_on_move(&channel.id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "sidebar sweep failed");
                AppError::boxed(
                    "MoveChannel",
                    "app.channel.sidebar_categories.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        channel.team_id = team.id.clone();
        self.store()
            .channel()
            .update(channel)
            .await
            .map_err(crate::channel_write::update_channel_error)?;

        // The three re-homings Go only logs.
        match self
            .store()
            .webhook()
            .get_incoming_by_team_by_user(&previous_team.id, "", 0, ALL_MEMBERS)
            .await
        {
            Ok(hooks) => {
                for mut hook in hooks {
                    if hook.channel_id == channel.id {
                        hook.team_id = team.id.clone();
                        if let Err(err) = self.store().webhook().update_incoming(&hook).await {
                            tracing::warn!(error = %err, hook_id = %hook.id, "Failed to move incoming webhook to new team");
                        }
                    }
                }
            }
            Err(err) => tracing::warn!(error = %err, "Failed to get incoming webhooks"),
        }
        match self
            .store()
            .webhook()
            .get_outgoing_by_team_by_user(&previous_team.id, "", 0, ALL_MEMBERS)
            .await
        {
            Ok(hooks) => {
                for mut hook in hooks {
                    if hook.channel_id == channel.id {
                        hook.team_id = team.id.clone();
                        if let Err(err) = self.store().webhook().update_outgoing(&hook).await {
                            tracing::warn!(error = %err, hook_id = %hook.id, "Failed to move outgoing webhook to new team.");
                        }
                    }
                }
            }
            Err(err) => tracing::warn!(error = %err, "Failed to get outgoing webhooks"),
        }
        if let Err(err) = self
            .store()
            .thread()
            .update_team_id_for_channel_threads(&channel.id, &team.id)
            .await
        {
            tracing::warn!(error = %err, "error while updating threads after channel move");
        }

        // The sweep again, after the row changed; a failure is logged, a forward is the
        // caller's to act on.
        match self
            .remove_users_from_channel_not_member_of_team(user, channel, team)
            .await
        {
            Ok(MemberWrite::Done(())) => {}
            Ok(MemberWrite::Forward(why)) => return Ok(MemberWrite::Forward(why)),
            Err(err) => {
                tracing::warn!(error = %err, "error while removing non-team member users");
            }
        }

        self.post_channel_move_message(user, channel, &previous_team)
            .await;
        Ok(MemberWrite::Done(()))
    }

    /// Port of `App.postChannelMoveMessage` (app/channel.go:3937): the English
    /// `api.team.move_channel.success` with the **previous** team's name, as a
    /// `system_move_channel` post by the mover. Logged on failure, like the other system posts;
    /// see [`App::create_system_post`] for why the sentence is a literal.
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id))]
    async fn post_channel_move_message(
        &self,
        user: &User,
        channel: &Channel,
        previous_team: &Team,
    ) {
        let mut post = Post {
            channel_id: channel.id.clone(),
            message: format!(
                "This channel has been moved to this team from {}.",
                previous_team.name
            ),
            post_type: POST_TYPE_MOVE_CHANNEL.to_owned(),
            user_id: user.id.clone(),
            ..Post::default()
        };
        post.add_prop("username", serde_json::Value::String(user.username.clone()));
        self.post_system_message(post, channel).await;
    }
}
