//! Port of `App.ConvertGroupMessageToChannel` (app/channel.go:4291) and its three helpers —
//! the app half of `POST /api/v4/channels/{channel_id}/convert_to_channel`, which turns a
//! group message into a private channel on one of the members' common teams.
//!
//! # The conversion is an `UpdateChannel` with four things around it
//!
//! Validation (the team must be common to the members, the channel a GM, the converter a member,
//! and the private-channel shape valid); the update itself — type `P`, the team, the name and
//! display name — through [`App::update_channel`], with its `channel_updated` event; then, with
//! every failure only logged: the channel dropped from every sidebar and re-filed into each
//! member's `channels` category on the new team, and a `system_gm_to_channel` post naming the
//! converter and the members. Last, and an error again, the converter becomes channel admin.
//!
//! # `incorrect_team` is checked before `original_channel_not_gm`
//!
//! `GetDirectOrGroupMessageMembersCommonTeams` runs first and refuses anything that is not a DM
//! or GM with **its** 400 (`get_common_teams.incorrect_channel_type`), so the 404
//! `original_channel_not_gm` is reachable only for a **DM** — the one non-GM the common-teams
//! read accepts.

use mm_model::channel::{
    CHANNEL_GROUP_MAX_USERS, CHANNEL_TYPE_GROUP, CHANNEL_TYPE_PRIVATE, Channel,
    GroupMessageConversionRequestBody,
};
use mm_model::post::{POST_TYPE_GM_CONVERTED_TO_CHANNEL, Post};
use mm_model::user::User;
use mm_model::utils::{AppError, AppResult};
use mm_store::sidebar_category_store::SidebarCategoryStore;

use crate::App;
use crate::channel_member::MemberWrite;
use crate::common_teams::CommonTeams;
use crate::user::UserPage;

/// `utils.JoinList` (channels/utils/humanize.go:12) with the English `humanize.list_join`:
/// nothing, the one item, or `a, b and c`.
pub(crate) fn join_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [others @ .., last] => format!("{} and {last}", others.join(", ")),
    }
}

impl App {
    /// Port of `App.ConvertGroupMessageToChannel` (app/channel.go:4291). Returns the updated
    /// channel as the handler encodes it.
    ///
    /// The common-teams read is the same one `GET /channels/{id}/common_teams` serves, and it
    /// has a branch this port hands to Go — a bot among the members — which is the one
    /// [`MemberWrite::Forward`] here.
    #[tracing::instrument(skip_all, fields(channel_id = %request.channel_id, team_id = %request.team_id))]
    pub async fn convert_group_message_to_channel(
        &self,
        converted_by_user_id: &str,
        request: &GroupMessageConversionRequestBody,
    ) -> Result<MemberWrite<Channel>, Box<AppError>> {
        let original = self.get_channel(&request.channel_id).await?;

        match self
            .validate_for_convert_group_message_to_channel(converted_by_user_id, &original, request)
            .await?
        {
            MemberWrite::Done(()) => {}
            MemberWrite::Forward(why) => return Ok(MemberWrite::Forward(why)),
        }

        // `originalChannel.DeepCopy()` with the four fields the conversion sets.
        let mut updated = original.clone();
        updated.channel_type = CHANNEL_TYPE_PRIVATE.to_owned();
        updated.team_id = request.team_id.clone();
        updated.name = request.name.clone();
        updated.display_name = request.display_name.clone();
        self.update_channel(&mut updated).await?;

        // `GetUsersInChannelPage(…, PerPage: ChannelGroupMaxUsers, asAdmin: false)`.
        let users = self
            .get_users_in_channel_page(
                &request.channel_id,
                UserPage {
                    page: 0,
                    per_page: CHANNEL_GROUP_MAX_USERS as i64,
                    inactive: false,
                    active: false,
                },
            )
            .await?;

        // `_ = a.setSidebarCategoriesForConvertedGroupMessage(…)` and
        // `_ = a.postMessageForConvertGroupMessageToChannel(…)` — both discarded.
        if let Err(err) = self
            .set_sidebar_categories_for_converted_group_message(request, &users)
            .await
        {
            tracing::warn!(error = %err, "sidebar categories for the converted group message");
        }
        self.post_message_for_convert_group_message_to_channel(
            &updated,
            converted_by_user_id,
            &users,
        )
        .await;

        // the user conversion the GM becomes the channel admin.
        self.update_channel_member_scheme_roles(
            &request.channel_id,
            converted_by_user_id,
            false,
            true,
            true,
        )
        .await?;

        Ok(MemberWrite::Done(updated))
    }

    /// Port of `App.validateForConvertGroupMessageToChannel` (app/channel.go:4382).
    async fn validate_for_convert_group_message_to_channel(
        &self,
        converted_by_user_id: &str,
        original: &Channel,
        request: &GroupMessageConversionRequestBody,
    ) -> Result<MemberWrite<()>, Box<AppError>> {
        let common_teams = match self
            .get_direct_or_group_message_members_common_teams(&original.id)
            .await?
        {
            CommonTeams::Teams(teams) => teams,
            // Unreachable with no requesting user: Go's `nil, nil` arm needs one to be absent.
            CommonTeams::NotAMember => Vec::new(),
            CommonTeams::BotMember => {
                return Ok(MemberWrite::Forward(
                    "a bot among the members needs the plugin environment's exemption",
                ));
            }
        };
        if !common_teams.iter().any(|team| team.id == request.team_id) {
            return Err(AppError::boxed(
                "validateForConvertGroupMessageToChannel",
                "app.channel.group_message_conversion.incorrect_team",
                None,
                String::new(),
                400,
            ));
        }

        if original.channel_type != CHANNEL_TYPE_GROUP {
            return Err(AppError::boxed(
                "ConvertGroupMessageToChannel",
                "app.channel.group_message_conversion.original_channel_not_gm",
                None,
                String::new(),
                404,
            ));
        }

        // `GetChannelMember` is the 404 for a non-member; Go's `channelMember == nil` arm after
        // it is dead, since the store never returns a nil member without an error.
        self.get_channel_member(&request.channel_id, converted_by_user_id)
            .await?;

        // apply dummy changes to check validity
        let mut clone = original.clone();
        clone.channel_type = CHANNEL_TYPE_PRIVATE.to_owned();
        clone.name = request.name.clone();
        clone.display_name = request.display_name.clone();
        clone.is_valid()?;
        Ok(MemberWrite::Done(()))
    }

    /// Port of `App.setSidebarCategoriesForConvertedGroupMessage` (app/channel.go:4336).
    ///
    /// The channel leaves every sidebar first — only GM members could have it — then each
    /// member's categories on the new team are read: none is normal (the defaults are created on
    /// first login) and skipped; otherwise the first category, the `channels` one, is re-saved
    /// as read, since the read auto-fills every channel of the user's that no category holds,
    /// which now includes this one. Each member's failure is logged.
    async fn set_sidebar_categories_for_converted_group_message(
        &self,
        request: &GroupMessageConversionRequestBody,
        users: &[User],
    ) -> AppResult<()> {
        // `DeleteAllSidebarChannelForChannel` is the same statement as
        // `UpdateSidebarChannelCategoryOnMove`: one `DELETE` by channel id.
        self.store()
            .sidebar_category()
            .update_sidebar_channel_category_on_move(&request.channel_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "sidebar sweep failed");
                AppError::boxed(
                    "setSidebarCategoriesForConvertedGroupMessage",
                    "app.channel.gm_conversion_set_categories.delete_all.error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        for user in users {
            let categories = match self
                .get_sidebar_categories_for_team_for_user(&user.id, &request.team_id)
                .await
            {
                Ok(categories) => categories,
                Err(err) => {
                    tracing::error!(error = %err, user_id = %user.id, "Failed to search sidebar categories for user for adding converted GM");
                    continue;
                }
            };
            let Some(channels_category) = categories
                .categories
                .as_ref()
                .and_then(|categories| categories.first())
            else {
                continue;
            };
            if let Err(err) = self
                .update_sidebar_categories(
                    &user.id,
                    &request.team_id,
                    std::slice::from_ref(channels_category),
                )
                .await
            {
                tracing::error!(error = %err, user_id = %user.id, "Failed to add converted GM to default sidebar category for user");
            }
        }
        Ok(())
    }

    /// Port of `App.postMessageForConvertGroupMessageToChannel` (app/channel.go:4433): the
    /// English `api.channel.group_message.converted.to_private_channel` — the converter's
    /// username and the members' as a humanised list — with the two props the client rebuilds
    /// a localised sentence from. Logged on failure; see [`App::create_system_post`].
    async fn post_message_for_convert_group_message_to_channel(
        &self,
        channel: &Channel,
        converted_by_user_id: &str,
        users: &[User],
    ) {
        let converted_by = match self.get_user(converted_by_user_id).await {
            Ok(user) => user,
            Err(err) => {
                tracing::error!(error = %err, "Failed to create post for notifying about GM converted to private channel");
                return;
            }
        };
        let usernames: Vec<&str> = users.iter().map(|user| user.username.as_str()).collect();
        let user_ids: Vec<serde_json::Value> = users
            .iter()
            .map(|user| serde_json::Value::String(user.id.clone()))
            .collect();

        let mut post = Post {
            channel_id: channel.id.clone(),
            message: format!(
                "{} created this channel from a group message with {}.",
                converted_by.username,
                join_list(&usernames)
            ),
            post_type: POST_TYPE_GM_CONVERTED_TO_CHANNEL.to_owned(),
            user_id: converted_by_user_id.to_owned(),
            ..Post::default()
        };
        // these props are used for re-constructing a localized message on the client
        post.add_prop(
            "convertedByUserId",
            serde_json::Value::String(converted_by.id.clone()),
        );
        post.add_prop(
            "gmMembersDuringConversionIDs",
            serde_json::Value::Array(user_ids),
        );
        self.post_system_message(post, channel).await;
    }
}

#[cfg(test)]
mod tests {
    use super::join_list;

    /// `JoinList`: the three shapes, and the Oxford comma is absent.
    #[test]
    fn join_list_humanises_like_go() {
        assert_eq!(join_list(&[]), "");
        assert_eq!(join_list(&["a"]), "a");
        assert_eq!(join_list(&["a", "b"]), "a and b");
        assert_eq!(join_list(&["a", "b", "c"]), "a, b and c");
    }
}
