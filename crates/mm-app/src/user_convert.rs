//! Account conversion — `App.ConvertUserToBot` (app/bot.go:648), `App.ConvertBotToUser`
//! (app/user.go:2942) and `App.PromoteGuestToUser` (app/user.go:2788), behind
//! `POST /api/v4/users/{user_id}/convert_to_bot`, `POST /api/v4/bots/{bot_user_id}/convert_to_user`
//! and `POST /api/v4/users/{user_id}/promote`.
//!
//! # A conversion is not symmetric, and neither direction is a transaction
//!
//! Going to a bot writes **one** row (`Bots`) and revokes the account's sessions; the `Users` row
//! is untouched, which is why `GET /users/{id}` answers with the same `roles`, the same
//! `create_at` and an `is_bot: true` that comes from a join rather than a column. Measured on the
//! stack: converting an account left its `Roles` at `system_user` and its `UpdateAt` unmoved by
//! this call.
//!
//! Coming back writes **four** things in order — roles (only for `set_system_admin`), the user
//! patch, the password, and then the `Bots` delete — with no transaction around them. Every
//! prefix of that sequence is a state the server can be left in, and one of them is reachable
//! from a client: a patch that applies followed by a password that fails validation leaves the
//! account patched, still a bot, and answering 400. Reproduced deliberately — see
//! [`App::convert_bot_to_user`]. A port that validated the password first would be better
//! software and would answer differently.
//!
//! # `ConvertUserToBot`'s first branch is not ported
//!
//! Go clears an account's federated credentials before making it a bot, through
//! `App.UpdateUserAuth` → `UserStore.UpdateAuthData`. Neither is in this tree, so the api layer
//! establishes `AuthService == ""` from the `GetUser` it has already made and hands the request
//! to Go otherwise — before anything is written. See `mm_api::user_convert` and [D-510].

use mm_model::bot::{Bot, bot_from_user};
use mm_model::session::{SESSION_PROP_IS_GUEST, Session};
use mm_model::user::{User, UserPatch};
use mm_model::utils::{AppError, AppResult};
use mm_model::websocket_message::{WEBSOCKET_EVENT_CHANNEL_MEMBER_UPDATED, WebSocketEvent};
use mm_store::{BotStore, SessionStore, StoreError, TeamStore, UserStore};

use crate::App;

/// `app.MissingAccountError` (app/user.go:47).
const MISSING_ACCOUNT_ERROR: &str = "app.user.missing_account.const";

impl App {
    /// Port of `App.ConvertUserToBot` (app/bot.go:648).
    ///
    /// # What it does *not* do is most of it
    ///
    /// No role change, no `Users` write, no deactivation, no check that the target is not already
    /// a bot and no check that the target is not the caller. An administrator can convert their
    /// own account and is logged out by the sessions revoke that follows — Go has no self-guard
    /// here and neither does this.
    ///
    /// # The already-a-bot branch is the primary key, not an `if`
    ///
    /// `Bots.UserId` is the primary key, so a second conversion is a unique-violation inside
    /// `Save`. `errors.As(err, &appErr)` misses a driver error, so the answer is the default arm:
    /// **500 `app.bot.createbot.internal_error`**, with `where` reading `CreateBot` even though
    /// no create was attempted. Measured on the stack; a port that pre-checked for the row would
    /// answer 400 and be wrong.
    ///
    /// # The bot in the answer is the one `Save` built, not the one a read would give
    ///
    /// `BotFromUser` sets `DisplayName` to `user.GetDisplayName(ShowUsername)` — the **username**
    /// — while [`mm_store::BotStore::get`] projects `Users.FirstName` into the same field. So
    /// `POST .../convert_to_bot` and the `GET /bots/{id}` that follows it report different
    /// display names for the same bot, and neither is wrong. Measured: `probecvt1` then `Probe`.
    ///
    /// # Precondition
    ///
    /// `user.auth_service` is empty — the caller has already established it and forwarded
    /// otherwise. See the module doc.
    #[tracing::instrument(skip_all, fields(user_id = %user.id))]
    pub async fn convert_user_to_bot(&self, user: &User) -> AppResult<Bot> {
        let bot = self
            .store()
            .bot()
            .save(&bot_from_user(user))
            .await
            .map_err(|err| match err {
                // Go's `errors.As(err, &appErr)` arm: `Save` returns `IsValid`'s `*model.AppError`
                // unwrapped, and it reaches the client with its own id and status.
                StoreError::Invalid { app_error, .. } => app_error,
                other => {
                    tracing::error!(error = %other, "converting the user to a bot failed");
                    AppError::boxed(
                        "CreateBot",
                        "app.bot.createbot.internal_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })?;

        // **After** the bot row, not before: a failed `Save` must leave the account logged in.
        self.revoke_all_sessions(&user.id).await?;

        Ok(bot)
    }

    /// Port of `App.ConvertBotToUser` (app/user.go:2942).
    ///
    /// # Five steps, no transaction, and the order is on the wire
    ///
    /// 1. `Store().User().Get(bot.UserId)` — the **store**, not `App.GetUser`, so a miss is
    ///    `app.user.missing_account.const` under `where = ConvertBotToUser`. Unreachable through
    ///    the route, whose `GetBot` already inner-joined that row.
    /// 2. `set_system_admin` **and** the account is not already an admin → `UpdateUserRoles` with
    ///    `"{existing} system_admin"` appended. Already-an-admin skips it, so the roles string
    ///    never grows a duplicate token. This runs *before* the patch, so a patch that fails
    ///    leaves the promotion done.
    /// 3. `user.Patch(userPatch)` then `UpdateUser(user, false)`.
    /// 4. `UpdatePassword` — which is where `model.user.is_valid.pwd_min_length.app_error` comes
    ///    from, measured, **after** step 3 has already been written.
    /// 5. `Bot().PermanentDelete` — the only step that stops the account being a bot.
    ///
    /// # The answer is stale in two fields, and that is Go's
    ///
    /// The returned user is the one `UpdateUser` produced, so `LastPasswordUpdate` still holds
    /// its pre-call value and `UpdateAt` predates the password write that bumped it again.
    /// `IsBot` is still `true`, because the struct was read from the join before step 5 removed
    /// the row. All three measured against the stack. Nothing here is sanitised: Go encodes the
    /// user straight out of `ConvertBotToUser`, unlike almost every other user-returning route.
    #[tracing::instrument(skip_all, fields(bot_user_id = %bot.user_id, set_system_admin = sysadmin))]
    pub async fn convert_bot_to_user(
        &self,
        bot: &Bot,
        patch: &UserPatch,
        sysadmin: bool,
        caller_session: Option<&Session>,
    ) -> AppResult<User> {
        let mut user = self.store().user().get(&bot.user_id).await.map_err(|err| {
            match err.is_not_found() {
                true => AppError::boxed(
                    "ConvertBotToUser",
                    MISSING_ACCOUNT_ERROR,
                    None,
                    String::new(),
                    404,
                ),
                false => {
                    tracing::error!(error = %err, "the bot's user row could not be read");
                    AppError::boxed(
                        "ConvertBotToUser",
                        "app.user.get.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            }
        })?;

        if sysadmin && !user.is_in_role(mm_model::user::external::SYSTEM_ADMIN_ROLE_ID) {
            let roles = format!(
                "{} {}",
                user.roles,
                mm_model::user::external::SYSTEM_ADMIN_ROLE_ID
            );
            // `sendWebsocketEvent = false`, and the returned user is discarded: Go keeps working
            // from the struct it read in step 1, which still carries the *old* roles. The
            // patch-and-update below then writes that stale list back — harmlessly, because
            // `UserStore.Update` with `trustedUpdateData = false` copies `Roles` off the stored
            // row and the stored row is the promoted one.
            self.update_user_roles(&user.id, &roles, false).await?;
        }

        user.patch(patch);

        let user = self.update_user(&user, false).await?;

        // The password Go has already proved non-empty at the api layer; its *validity* is
        // checked here, after the patch above has been written.
        self.update_password(
            caller_session,
            &user,
            patch.password.as_deref().unwrap_or_default(),
        )
        .await?;

        self.store()
            .bot()
            .permanent_delete(&bot.user_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "removing the bot row failed");
                AppError::boxed(
                    "ConvertBotToUser",
                    "app.user.convert_bot_to_user.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        Ok(user)
    }

    /// Port of `App.PromoteGuestToUser` (app/user.go:2788).
    ///
    /// # One error can fail it; everything after the roles write is best-effort
    ///
    /// Only the store call and `GetTeamsByUserId` return early. From there on Go *logs* and
    /// continues: a team whose default channels cannot be joined, a user that cannot be re-read,
    /// a session table that will not take the new roles, a team- or channel-member list that
    /// fails — each is a `Warn` and a 200. The single exception is the `json.Marshal` of a
    /// channel member, which returns `api.marshal_error`; a `ChannelMember` cannot fail to
    /// encode, so that branch is a log line here rather than a fabricated error.
    ///
    /// # `JoinDefaultChannels` is what makes promotion more than a role edit
    ///
    /// A guest is in exactly the channels it was invited to. Promotion adds it to every default
    /// channel of every team it belongs to — `shouldBeAdmin = false`, `userRequestorId` the
    /// **caller**, not the target. Those are the rows a client sees change.
    ///
    /// # The re-read is what the events carry
    ///
    /// `sendUpdatedUserEvent` and `UpdateSessionsIsGuest` both run against a *fresh* read taken
    /// after the write, so they carry `system_user`. Publishing the caller's stale struct would
    /// broadcast `system_guest` to every client that just watched the promotion succeed.
    #[tracing::instrument(skip_all, fields(user_id = %user.id, requestor_id = %requestor_id, teams))]
    pub async fn promote_guest_to_user(&self, user: &User, requestor_id: &str) -> AppResult<()> {
        self.store()
            .user()
            .promote_guest_to_user(&user.id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "promoting the guest failed");
                AppError::boxed(
                    "PromoteGuestToUser",
                    "app.user.promote_guest.user_update.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        let teams = self
            .store()
            .team()
            .get_teams_by_user_id(&user.id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "the promoted user's teams could not be listed");
                AppError::boxed(
                    "PromoteGuestToUser",
                    "app.team.get_all.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        tracing::Span::current().record("teams", teams.len());

        for team in &teams {
            // "Soft error if there is an issue joining the default channels" — Go's own comment.
            if let Err(err) = self
                .join_default_channels(&team.id, user, false, requestor_id)
                .await
            {
                tracing::warn!(
                    user_id = %user.id,
                    team_id = %team.id,
                    requestor_id = %requestor_id,
                    error = %err,
                    "Failed to join default channels",
                );
            }
        }

        match self.get_user(&user.id).await {
            Ok(promoted) => {
                self.send_updated_user_event(&promoted).await;
                let is_guest = promoted.is_guest();
                if let Err(err) = self.update_sessions_is_guest(&promoted, is_guest).await {
                    tracing::warn!(user_id = %promoted.id, error = %err, "Unable to update user sessions");
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "Failed to get user on promote guest to user");
            }
        }

        // `excludeTeamId = ""`, `includeDeleted = true` — the deleted memberships are included
        // deliberately, so a guest promoted after leaving a team still gets the event.
        let team_members = match self.get_team_members_for_user(&user.id, "", true).await {
            Ok(members) => members,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "Failed to get team members for user on promote guest to user",
                );
                Vec::new()
            }
        };

        for member in &team_members {
            self.send_updated_team_member_event(member).await;

            let channel_members = match self
                .get_channel_members_for_user(&member.team_id, &user.id)
                .await
            {
                Ok(members) => members,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "Failed to get channel members for user on promote guest to user",
                    );
                    // Go does **not** `continue` here — unlike `DemoteUserToGuest`, whose
                    // identical block does. The nil slice makes the loop below a no-op either
                    // way, so the difference is invisible; it is kept because the two functions
                    // really do differ and a reader comparing them should not think this a slip.
                    Vec::new()
                }
            };

            for channel_member in &channel_members {
                let mut event = WebSocketEvent::new(
                    WEBSOCKET_EVENT_CHANNEL_MEMBER_UPDATED,
                    "",
                    "",
                    &user.id,
                    None,
                    "",
                );
                match serde_json::to_string(channel_member) {
                    Ok(json) => event.add("channelMember", serde_json::Value::String(json)),
                    Err(err) => {
                        tracing::warn!(error = %err, "failed to encode a ChannelMember for the socket");
                        continue;
                    }
                }
                self.publish(event).await;
            }
        }

        Ok(())
    }

    /// Port of `PlatformService.UpdateSessionsIsGuest` (app/platform/session.go:299).
    ///
    /// # The roles write is unscoped and comes first
    ///
    /// `Session().UpdateRoles(user.Id, user.GetRawRoles())` rewrites **every** session of the
    /// user in one statement, before the per-session prop loop. So a promotion reaches the
    /// account's phone and desktop, not just the browser that asked for it.
    ///
    /// A session whose prop write fails is **skipped, not fatal** — Go logs and `continue`s — and
    /// the function still returns `nil`. Its one caller only logs the result anyway.
    #[tracing::instrument(skip_all, fields(user_id = %user.id, is_guest, sessions))]
    async fn update_sessions_is_guest(&self, user: &User, is_guest: bool) -> AppResult<()> {
        let sessions = self.get_sessions(&user.id).await?;
        tracing::Span::current().record("sessions", sessions.len());

        self.store()
            .session()
            .update_roles(&user.id, user.get_raw_roles())
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "updating the session roles failed");
                AppError::boxed(
                    "UpdateSessionsIsGuest",
                    "app.session.analytics_session_count.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        for session in sessions {
            let mut session = session;
            session.add_prop(SESSION_PROP_IS_GUEST, is_guest.to_string());
            if let Err(err) = self.store().session().update_props(&session).await {
                tracing::warn!(error = %err, "Unable to update isGuest session");
                continue;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    /// `BotFromUser` takes the **username** for the display name, which is not what a read of the
    /// same bot returns. Stated here because the two are one `git grep` apart and look like a bug.
    #[test]
    fn a_converted_bot_carries_the_username_as_its_display_name() {
        let mut user = mm_model::user::User {
            id: "chckdesc8i8nigphx13d17jg7c".to_owned(),
            username: "probecvt1".to_owned(),
            first_name: "Probe".to_owned(),
            ..Default::default()
        };
        user.last_name = "One".to_owned();

        let bot = mm_model::bot::bot_from_user(&user);

        assert_eq!(bot.display_name, "probecvt1", "measured against the stack");
        assert_eq!(bot.owner_id, user.id, "the account owns itself");
        assert_eq!(bot.user_id, user.id);
        assert!(bot.description.is_empty(), "omitted on the wire");
    }
}
