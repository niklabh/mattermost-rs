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
//! # The webhooks are re-homed through the *app* page reads, so their settings gate them
//!
//! Go reads the old team's hooks with `GetIncomingWebhooksForTeamPage` and
//! `GetOutgoingWebhooksForTeamPage`, which refuse with a 501 when
//! `ServiceSettings.EnableIncomingWebhooks`/`EnableOutgoingWebhooks` is off. The move logs that
//! and carries on, so **with a kind of webhook disabled its hooks stay on the old team** while
//! the channel moves. The store is not asked directly for that reason. Each re-homed hook gets a
//! fresh `UpdateAt` from the store's own stamp.
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

/// The i18n id of the move notice (en.json: `This channel has been moved to this team from %v.`).
const MOVE_CHANNEL_SUCCESS: &str = "api.team.move_channel.success";

/// Go's `fmt.Sprintf(format, arg)` for one **string** operand, as far as a translated sentence
/// can exercise it: `%%` is a literal `%`, the first `%v` is `arg`, a later `%v` is
/// `%!v(MISSING)`, and an operand no verb consumed is appended as `%!(EXTRA string=arg)`.
///
/// Every shipped translation of `api.team.move_channel.success` — all 22 supported locales —
/// carries exactly one `%v` and no other `%`, so only the first rule is reached in practice; the
/// others are Go's answers for a translation that drifts. Any other verb is copied verbatim,
/// which Go would not do, and none occurs.
pub(crate) fn go_sprintf_v(format: &str, arg: &str) -> String {
    let mut out = String::with_capacity(format.len() + arg.len());
    let mut used = false;
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('%') => {
                chars.next();
                out.push('%');
            }
            Some('v') => {
                chars.next();
                if used {
                    out.push_str("%!v(MISSING)");
                } else {
                    out.push_str(arg);
                    used = true;
                }
            }
            _ => out.push('%'),
        }
    }
    if !used {
        out.push_str("%!(EXTRA string=");
        out.push_str(arg);
        out.push(')');
    }
    out
}

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
    /// `GetTeamMembersByIds` returns only the ids with a **live** row — the store filters
    /// `TeamMembers.DeleteAt = 0` (team_store.go:1164), so a member who once left the target team
    /// counts as not in it — and the comparison is by count and then by set.
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id, team_id = %team.id))]
    pub async fn remove_users_from_channel_not_member_of_team(
        &self,
        remover: Option<&User>,
        channel: &Channel,
        team: &Team,
        hook_ctx: &crate::plugin_hooks::HookContext,
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
                // `removerId` is the empty string when there is no remover — the local-mode
                // move — and that is what the removal's websocket events then carry.
                .remove_user_from_channel_inner(
                    user_id,
                    remover.map_or("", |remover| remover.id.as_str()),
                    channel,
                    hook_ctx,
                )
                .await?
            {
                MemberWrite::Done(_) => {}
                MemberWrite::Forward(why) => return Ok(MemberWrite::Forward(why)),
            }
        }
        Ok(MemberWrite::Done(()))
    }

    /// Port of `App.MoveChannel` (app/channel.go:3822). `channel` is updated in place — the
    /// handler encodes it, `TeamId` and the store's `UpdateAt` included. Never forwards: its
    /// one membership sweep logs what it cannot do (see the comment at the sweep).
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id, team_id = %team.id))]
    pub async fn move_channel(
        &self,
        team: &Team,
        channel: &mut Channel,
        user: Option<&User>,
        hook_ctx: &crate::plugin_hooks::HookContext,
    ) -> AppResult<()> {
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

        // The three re-homings Go only logs. `page 0, per_page 10000000` — offset 0.
        match self
            .get_incoming_webhooks_for_team_page_by_user(&previous_team.id, "", 0, ALL_MEMBERS)
            .await
        {
            Ok(hooks) => {
                for mut hook in hooks {
                    if hook.channel_id == channel.id {
                        hook.team_id = team.id.clone();
                        if let Err(err) = self.store().webhook().update_incoming(&mut hook).await {
                            tracing::warn!(error = %err, hook_id = %hook.id, "Failed to move incoming webhook to new team");
                        }
                    }
                }
            }
            Err(err) => tracing::warn!(error = %err, "Failed to get incoming webhooks"),
        }
        match self
            .get_outgoing_webhooks_for_team_page_by_user(&previous_team.id, "", 0, ALL_MEMBERS)
            .await
        {
            Ok(hooks) => {
                for mut hook in hooks {
                    if hook.channel_id == channel.id {
                        hook.team_id = team.id.clone();
                        if let Err(err) = self.store().webhook().update_outgoing(&mut hook).await {
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

        // The sweep again, after the row changed; a failure is logged, as Go logs it.
        //
        // A member this port cannot remove (see `remove_user_from_channel_inner`) is logged
        // too, and **not** forwarded: the channel row has already moved, so Go re-running the
        // whole request would find the *new* team as the previous one and post a second notice
        // naming it. The precondition above has just proved every member is on the new team, so
        // this is reachable only by a join racing the move — the window Go's own comment on
        // `MoveChannel` concedes.
        match self
            .remove_users_from_channel_not_member_of_team(user, channel, team, hook_ctx)
            .await
        {
            Ok(MemberWrite::Done(())) => {}
            Ok(MemberWrite::Forward(why)) => {
                tracing::warn!(
                    reason = why,
                    "a member who joined during the move was left on the channel"
                );
            }
            Err(err) => {
                tracing::warn!(error = %err, "error while removing non-team member users");
            }
        }

        // `if user != nil`: the local-mode move posts no notice at all.
        if let Some(user) = user {
            self.post_channel_move_message(hook_ctx, user, channel, &previous_team)
                .await;
        }
        Ok(())
    }

    /// Port of `App.postChannelMoveMessage` (app/channel.go:3935):
    /// `fmt.Sprintf(i18n.T("api.team.move_channel.success"), previousTeam.Name)` — the
    /// **previous** team's name — as a `system_move_channel` post by the mover. Logged on
    /// failure, like the other system posts.
    ///
    /// `i18n.T` is the **server** locale's function (`GetTranslationsBySystemLocale`), not the
    /// mover's, so a German `DefaultServerLocale` writes the German sentence whatever the
    /// admin's own locale — [`Translations::server_locale`](crate::i18n::Translations::server_locale)
    /// over the bundle. The sentence is then a Go format string, not a template, and
    /// [`go_sprintf_v`] applies it.
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id))]
    async fn post_channel_move_message(
        &self,
        ctx: &crate::plugin_hooks::HookContext,
        user: &User,
        channel: &Channel,
        previous_team: &Team,
    ) {
        let format = match crate::i18n::translations().await {
            Some(bundle) => bundle.translate(
                bundle.server_locale(&self.config().default_server_locale),
                MOVE_CHANNEL_SUCCESS,
            ),
            // No bundle is a server Go refuses to start (`i18n::init`); `en.json`'s sentence.
            None => "This channel has been moved to this team from %v.".to_owned(),
        };
        let mut post = Post {
            channel_id: channel.id.clone(),
            message: go_sprintf_v(&format, &previous_team.name),
            post_type: POST_TYPE_MOVE_CHANNEL.to_owned(),
            user_id: user.id.clone(),
            ..Post::default()
        };
        post.add_prop("username", serde_json::Value::String(user.username.clone()));
        self.post_system_message(ctx, post, channel).await;
    }
}

#[cfg(test)]
mod tests {
    use super::go_sprintf_v;

    /// Transcribed from Go's `fmt` package documentation (`%!v(MISSING)`, `%!(EXTRA …)`); no
    /// fixture: the shipped translations only ever reach the first case.
    #[test]
    fn go_sprintf_v_follows_fmt() {
        assert_eq!(
            go_sprintf_v("This channel has been moved to this team from %v.", "alpha"),
            "This channel has been moved to this team from alpha."
        );
        assert_eq!(
            go_sprintf_v("此频道已从 %v 移至此团队。", "b"),
            "此频道已从 b 移至此团队。"
        );
        assert_eq!(go_sprintf_v("100%% from %v", "x"), "100% from x");
        assert_eq!(go_sprintf_v("%v and %v", "x"), "x and %!v(MISSING)");
        assert_eq!(go_sprintf_v("moved", "x"), "moved%!(EXTRA string=x)");
        // The operand is inserted, never re-scanned.
        assert_eq!(go_sprintf_v("from %v.", "50%v"), "from 50%v.");
    }

    /// Every supported locale's sentence has the one `%v` the port relies on.
    #[test]
    fn every_shipped_move_sentence_has_one_operand() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../reference/mattermost/server/i18n");
        if !dir.is_dir() {
            return;
        }
        for locale in crate::i18n::SUPPORTED_LOCALES {
            let text = std::fs::read_to_string(dir.join(format!("{locale}.json"))).unwrap();
            let entries: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
            let Some(sentence) = entries
                .iter()
                .find(|e| e["id"] == super::MOVE_CHANNEL_SUCCESS)
                .and_then(|e| e["translation"].as_str())
            else {
                continue;
            };
            assert_eq!(sentence.matches('%').count(), 1, "{locale}: {sentence}");
            assert_eq!(sentence.matches("%v").count(), 1, "{locale}: {sentence}");
        }
    }
}
