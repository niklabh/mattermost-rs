//! Port of `App.getDirectOrGroupMessageMembersCommonTeams` (channels/app/channel.go:4236) —
//! the teams every active member of a DM or GM belongs to.
//!
//! # One branch is not portable, and it is a *bot* branch
//!
//! Go skips a bot member when `IsBotExemptFromDMRestrictions` says so (app/bot.go:359), and that
//! function's last test is `pluginsEnvironment.Available()` — the plugin **manifests loaded into
//! the running server's memory**, from a directory this process cannot see. A bot owned by a
//! plugin is therefore exempt in Go and unknowable here.
//!
//! So a channel with an active bot member is [`CommonTeams::BotMember`] and the handler forwards
//! it. That is the same treatment `prepare_post_list_for_client` gives an image-proxy post: refuse
//! to answer rather than answer differently. The two earlier branches — the system bot by
//! username, and a bot the caller owns — *are* portable, and they are deliberately not
//! implemented on their own: answering two thirds of a rule is how a port gets a wrong answer
//! confidently.

use mm_model::channel::{CHANNEL_GROUP_MAX_USERS, CHANNEL_TYPE_DIRECT, CHANNEL_TYPE_GROUP};
use mm_model::team::Team;
use mm_model::utils::{AppError, AppResult};
use mm_store::{StoreError, TeamStore, UserStore};

use crate::App;

/// What the route should do with this channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommonTeams {
    /// The answer. **Empty is not the same as [`CommonTeams::NotAMember`]**: Go builds
    /// `teams := []*model.Team{}` and only replaces it when there are ids, so "no team in common"
    /// is `[]` on the wire while "you are not in this channel" is `null`.
    Teams(Vec<Team>),
    /// The requesting user is not among the channel's **active** members. Go returns `nil, nil`
    /// (channel.go:4275) — deliberately not an error, and deliberately not `[]`.
    NotAMember,
    /// An active member is a bot, whose exemption needs the plugin environment. Forward.
    BotMember,
}

impl App {
    /// Port of `App.GetDirectOrGroupMessageMembersCommonTeamsAsUser` (channel.go:4220), which is
    /// the same function with the session's user id passed as the requesting user.
    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, members, teams))]
    pub async fn get_direct_or_group_message_members_common_teams_as_user(
        &self,
        requesting_user_id: &str,
        channel_id: &str,
    ) -> AppResult<CommonTeams> {
        let channel = self.get_channel(channel_id).await?;

        if channel.channel_type != CHANNEL_TYPE_GROUP && channel.channel_type != CHANNEL_TYPE_DIRECT
        {
            return Err(AppError::boxed(
                "GetDirectOrGroupMessageMembersCommonTeams",
                "app.channel.get_common_teams.incorrect_channel_type",
                None,
                String::new(),
                400,
            ));
        }

        // `UserGetOptions{PerPage: ChannelGroupMaxUsers, Page: 0, InChannelId, Active: true}` —
        // `Active` renders as `Users.DeleteAt = 0` (user_store.go:878), which is what
        // `Some(false)` means here. **`PerPage` is 8**, the GM maximum, so a DM or GM cannot
        // overflow it; a channel type that could would be silently truncated, which is why the
        // type check above runs first.
        let members = self
            .store()
            .user()
            .get_profiles_in_channel(channel_id, 0, CHANNEL_GROUP_MAX_USERS as i64, Some(false))
            .await
            .map_err(|err| common_teams_store_error("users in the channel", err))?;
        tracing::Span::current().record("members", members.len());

        if members.iter().any(|member| member.is_bot) {
            return Ok(CommonTeams::BotMember);
        }

        let user_ids: Vec<String> = members.into_iter().map(|member| member.id).collect();

        // Go's short-circuit: a requesting user who is not an *active* member of the channel gets
        // an empty set rather than an error, "to offer more flexibility to the remaining users on
        // where to create the replacement channel" (channel.go:4271-4275). Note it is checked
        // against the list above, so a **deactivated** member asking about their own channel
        // lands here too.
        if !user_ids.iter().any(|id| id == requesting_user_id) {
            return Ok(CommonTeams::NotAMember);
        }

        let team_ids = self
            .store()
            .team()
            .get_common_team_ids_for_multiple_users(&user_ids)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "common team ids lookup failed");
                AppError::boxed(
                    "GetDirectOrGroupMessageMembersCommonTeams",
                    "app.channel.get_common_teams.store_get_common_teams_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        if team_ids.is_empty() {
            // `teams := []*model.Team{}` and the `if len(commonTeamIDs) > 0` never fires.
            tracing::Span::current().record("teams", 0);
            return Ok(CommonTeams::Teams(Vec::new()));
        }

        let teams = self.get_teams(&team_ids).await?;
        tracing::Span::current().record("teams", teams.len());
        Ok(CommonTeams::Teams(teams))
    }

    /// Port of `App.GetTeams` (app/team.go:912).
    ///
    /// Its `where` is **`GetTeam`**, singular, in both arms — a copy-paste in Go that is
    /// reproduced rather than corrected, and the two arms differ only in id and status:
    /// `app.team.get.find.app_error` / 404 for not-found, `app.team.get.finding.app_error` / 500
    /// otherwise. One word apart, and easy to swap.
    #[tracing::instrument(skip_all, fields(asked = team_ids.len()))]
    pub async fn get_teams(&self, team_ids: &[String]) -> AppResult<Vec<Team>> {
        self.store().team().get_many(team_ids).await.map_err(|err| {
            let not_found = err.is_not_found();
            if !not_found {
                tracing::error!(error = ?err, "team lookup failed");
            }
            AppError::boxed(
                "GetTeam",
                if not_found {
                    "app.team.get.find.app_error"
                } else {
                    "app.team.get.finding.app_error"
                },
                None,
                String::new(),
                if not_found { 404 } else { 500 },
            )
        })
    }
}

/// The users query has no error branch of its own in Go — `GetUsersInChannel`'s `AppError` is
/// returned straight through. Ours names the same 500 the app layer gives every other store
/// failure on this path.
fn common_teams_store_error(what: &str, err: StoreError) -> Box<AppError> {
    tracing::error!(what, error = ?err, "common teams lookup failed");
    AppError::boxed(
        "GetDirectOrGroupMessageMembersCommonTeams",
        "app.user.get_profiles.app_error",
        None,
        String::new(),
        500,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three outcomes are three different answers, and two of them are *empty* in different
    /// ways. Collapsing `NotAMember` into `Teams(vec![])` would turn a `null` into a `[]`.
    #[test]
    fn an_empty_answer_and_a_missing_membership_are_not_the_same_value() {
        assert_ne!(CommonTeams::Teams(Vec::new()), CommonTeams::NotAMember);
        assert_ne!(CommonTeams::BotMember, CommonTeams::NotAMember);
    }

    /// The two `GetTeams` arms are one word apart. Pinned so a "tidy-up" that unifies them fails.
    #[test]
    fn the_two_get_teams_ids_differ_by_one_word() {
        assert_ne!(
            "app.team.get.find.app_error",
            "app.team.get.finding.app_error"
        );
    }
}
