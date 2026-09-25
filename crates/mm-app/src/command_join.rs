//! Port of the `/join` built-in slash command (app/slashcommands/command_join.go) and of
//! `App.JoinChannel` (app/channel.go:2719), the second call site of `UserHasJoinedChannel`.
//!
//! `/join` is the one built-in provider this server runs. Every other built-in still forwards
//! ([D-781]); `/join` is here because it is the only caller of `JoinChannel` a client reaches, and
//! with it the last unported site of a hook that already fires ([D-932]).

use mm_model::channel::Channel;
use mm_model::command_args::CommandArgs;
use mm_model::command_response::{COMMAND_RESPONSE_TYPE_EPHEMERAL, CommandResponse};
use mm_model::permission::{PERMISSION_JOIN_PUBLIC_CHANNELS, PERMISSION_READ_CHANNEL};
use mm_model::utils::AppError;
use mm_store::channel_store::ChannelStore;
use mm_store::user_store::UserStore;

use crate::App;
use crate::channel_member::MemberWrite;
use crate::plugin_hooks::HookContext;

/// `model.MissingAccountError`.
const MISSING_ACCOUNT_ERROR: &str = "app.user.missing_account.const";

/// What `JoinChannel` did: joined (or found the user already a member), or met a branch this
/// server hands to Go — decided before anything is written.
#[derive(Debug)]
pub enum JoinOutcome {
    Joined,
    Forward(&'static str),
}

impl App {
    /// Port of `App.JoinChannel` (app/channel.go:2719).
    ///
    /// The user is read first and its failure decides the error even when the member read would
    /// also fail — Go reads both concurrently and looks at the user's result first. An existing
    /// membership is success with nothing done; only then is a non-open channel refused. After
    /// `AddUserToChannel`, `UserHasJoinedChannel` is spawned with a **nil** actor, and the
    /// join post's failure is the call's failure. Unlike the add-member route, there is no
    /// `IsSpace` return between the add and the hook: `AddUserToChannel`'s own return for a space
    /// only skips its events.
    pub async fn join_channel(
        &self,
        ctx: &HookContext,
        channel: &Channel,
        user_id: &str,
    ) -> Result<JoinOutcome, Box<AppError>> {
        let user = match self.store().user().get(user_id).await {
            Ok(user) => user,
            Err(err) if err.is_not_found() => {
                return Err(Box::new(
                    AppError::new(
                        "CreateChannel",
                        MISSING_ACCOUNT_ERROR,
                        None,
                        String::new(),
                        404,
                    )
                    .wrap(err),
                ));
            }
            Err(err) => {
                return Err(Box::new(
                    AppError::new(
                        "CreateChannel",
                        "app.user.get.app_error",
                        None,
                        String::new(),
                        500,
                    )
                    .wrap(err),
                ));
            }
        };
        if self
            .store()
            .channel()
            .get_member(&channel.id, user_id)
            .await
            .is_ok()
        {
            return Ok(JoinOutcome::Joined);
        }
        if channel.channel_type != mm_model::channel::CHANNEL_TYPE_OPEN {
            return Err(AppError::boxed(
                "JoinChannel",
                "api.channel.join_channel.permissions.app_error",
                None,
                String::new(),
                400,
            ));
        }
        let member = match self.add_user_to_channel(&user, channel, false, ctx).await? {
            MemberWrite::Done(member) => member,
            MemberWrite::Forward(why) => return Ok(JoinOutcome::Forward(why)),
        };
        self.user_has_joined_channel(ctx, &member, None);
        self.post_join_channel_message(ctx, &user, channel).await?;
        Ok(JoinOutcome::Joined)
    }

    /// Port of `JoinProvider.DoCommand` (command_join.go:44). `None` is a branch handed to Go
    /// ([`JoinOutcome::Forward`]); nothing has been written when it is answered.
    ///
    /// The name is lower-cased **unless** it starts with `~`, which is only stripped — so
    /// `/join ~Town-Square` looks up `Town-Square` and finds nothing, as in Go. The lookup
    /// **excludes** archived channels — the `true` Go passes to `GetByName` is `allowFromCache`,
    /// not "include deleted", so an archived channel is `list.app_error` — and a match whose
    /// stored name differs from the one asked for is `missing`. The permission is the **user's**, not the session's: `join_public_channels`
    /// for an open channel, `read_channel` for a private one, and any other type fails. Every
    /// failure is an ephemeral text in the request's language; success is only a `goto_location`.
    pub async fn do_join_command(
        &self,
        ctx: &HookContext,
        args: &CommandArgs,
        message: &str,
    ) -> Option<CommandResponse> {
        let t = |id: &str| match crate::i18n::loaded() {
            Some(bundle) => bundle.translate_for_request(
                &ctx.accept_language,
                &self.config().default_client_locale,
                id,
            ),
            None => id.to_owned(),
        };
        let ephemeral = |id: &str| {
            Some(CommandResponse {
                text: t(id),
                response_type: COMMAND_RESPONSE_TYPE_EPHEMERAL.to_owned(),
                ..CommandResponse::default()
            })
        };
        const FAIL: &str = "api.command_join.fail.app_error";

        let channel_name = match message.strip_prefix('~') {
            Some(name) => name.to_owned(),
            None => message.to_lowercase(),
        };
        let Ok(channel) = self
            .store()
            .channel()
            .get_by_name(&args.team_id, &channel_name, false)
            .await
        else {
            return ephemeral("api.command_join.list.app_error");
        };
        if channel.name != channel_name {
            return ephemeral("api.command_join.missing.app_error");
        }
        let permission = match channel.channel_type.as_str() {
            mm_model::channel::CHANNEL_TYPE_OPEN => &PERMISSION_JOIN_PUBLIC_CHANNELS,
            mm_model::channel::CHANNEL_TYPE_PRIVATE => &PERMISSION_READ_CHANNEL,
            _ => return ephemeral(FAIL),
        };
        if !self
            .has_permission_to_channel(&args.user_id, &channel.id, permission)
            .await
            .0
        {
            return ephemeral(FAIL);
        }
        match self.join_channel(ctx, &channel, &args.user_id).await {
            Ok(JoinOutcome::Joined) => {}
            Ok(JoinOutcome::Forward(why)) => {
                tracing::debug!(why, "/join forwarded");
                return None;
            }
            Err(err) => {
                tracing::debug!(error = %err, "/join failed");
                return ephemeral(FAIL);
            }
        }
        let Ok(team) = self.get_team(&channel.team_id).await else {
            return ephemeral(FAIL);
        };
        Some(CommandResponse {
            goto_location: format!("{}/{}/channels/{}", args.site_url, team.name, channel.name),
            ..CommandResponse::default()
        })
    }
}
