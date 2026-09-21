//! Deactivation — `App.UpdateActive(user, false)` and the tail that only runs on that arm
//! (app/user.go:1229, :1172), behind `DELETE /api/v4/users/{user_id}` and
//! `PUT /api/v4/users/{user_id}/active`.
//!
//! # There is no prefix of a deactivation that can be served
//!
//! Everything `UpdateActive` does for `active = false` runs **after** the `UPDATE`: the session
//! revocation, the status write, the OAuth sweep, the bot cascade, the sysadmin DM and the plugin
//! hook. A request handed to Go part-way through would leave a deactivated row whose sessions
//! were never revoked — a logged-in account with `DeleteAt != 0`, which no code path in either
//! server produces. So the decision to serve or forward is taken from reads alone, before
//! [`App::deactivate_user`] is called at all. That was [D-461]; this module is its answer.
//!
//! # What decides it: whether the account owns a bot
//!
//! `userDeactivated` (app/user.go:1172) runs, in order:
//!
//! 1. `SetStatusOffline(id, false, true)` — ported ([`App::set_status_offline`]).
//! 2. `notifySysadminsBotOwnerDeactivated` — **returns `nil` immediately when the user owns no
//!    bots** (app/user.go:568). Otherwise it DMs every system administrator a message built from
//!    `app.bot.get_disable_bot_sysadmin_message`, an i18n template with two conditionals in it,
//!    through `GetOrCreateDirectChannel` and `CreatePost`.
//! 3. `disableUserBots` when `ServiceSettings.DisableBotsWhenOwnerIsDeactivated` — also a no-op
//!    for an owner of no bots, since its `GetBots` page comes back empty.
//! 4. The two `OAuthStore` deletes — ported ([`mm_store::OAuthStore::remove_auth_data_by_user_id`]
//!    and [`mm_store::OAuthStore::permanent_delete_auth_data_by_user`]).
//!
//! So steps 2 and 3 are exactly the part this process cannot reproduce, and both are gated on the
//! same fact — one that is a `SELECT` away and knowable before the write.
//! [`App::owns_bots`] asks it; a caller that gets `true` must forward the whole request.
//! There is deliberately no "deactivate and skip the bots" path: a sysadmin DM that Go sends and
//! this does not is a row missing from `Posts`, which is a divergence a response body cannot show.
//!
//! # What is still missing on the served arm
//!
//! - `InvalidateCacheForUser` and `invalidateUserChannelMembersCaches` — in-process caches Go
//!   keeps and this process does not have. No row, no byte.
//!
//! # `PermanentDeleteUser` is a deactivation first
//!
//! [`App::permanent_delete_user`] opens with `UpdateActive(user, false)`, so everything above
//! applies to it unchanged: it is served only for an owner of no bots, and the caller decides
//! that from reads before anything is written. [`App::permanent_delete_needs_go`] is that
//! decision for the one-user routes and [`App::permanent_delete_all_needs_go`] for
//! `localPermanentDeleteAllUsers`, where the question is subtler — see its doc.

use std::collections::HashMap;

use mm_model::bot::BotGetOptions;
use mm_model::file_info::FileInfo;
use mm_model::role::SYSTEM_ADMIN_ROLE_ID;
use mm_model::user::User;
use mm_model::utils::{AppError, AppResult};
use mm_store::{
    AuditStore, BotStore, ChannelStore, CommandStore, DraftStore, FileInfoStore, GroupStore,
    OAuthStore, PostStore, PreferenceStore, ReactionStore, ScheduledPostStore, SessionStore,
    StoreError, TeamStore, UserAccessTokenStore, UserStore, WebhookStore,
};

use crate::App;
use crate::plugin_hooks::HookContext;

impl App {
    /// Whether this account owns at least one bot that is not soft-deleted.
    ///
    /// Go asks the same question twice on a deactivation, both times through `GetBots` with
    /// `IncludeDeleted: false` — `notifySysadminsBotOwnerDeactivated` pages at 25 and
    /// `disableUserBots` at 20, and both stop at the first empty page. Only the *emptiness* of
    /// the first page is load-bearing for the forward decision, so this asks for one row.
    ///
    /// `OnlyOrphaned` is false, matching both call sites: a bot whose owner is being deactivated
    /// is by definition not orphaned yet.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, owns_bots))]
    pub async fn owns_bots(&self, user_id: &str) -> AppResult<bool> {
        let bots = self
            .get_bots(&BotGetOptions {
                owner_id: user_id.to_owned(),
                include_deleted: false,
                only_orphaned: false,
                page: 0,
                per_page: 1,
            })
            .await?;
        let owns = !bots.0.is_empty();
        tracing::Span::current().record("owns_bots", owns);
        Ok(owns)
    }

    /// Port of `App.UpdateActive` (app/user.go:1229), **deactivation only** — the sibling of
    /// [`App::activate_user`](crate::App::activate_user).
    ///
    /// # `DeleteAt` is seeded from an `UpdateAt` the store then throws away
    ///
    /// Go writes `user.UpdateAt = GetMillis()` and then `user.DeleteAt = user.UpdateAt`
    /// (app/user.go:1243) — and `SqlUserStore.Update` calls `user.PreUpdate()`, whose
    /// `u.UpdateAt = GetMillis()` (model/user.go:563) **overwrites it**. So the assignment's only
    /// lasting effect is the value `DeleteAt` copied off it, and the stored row satisfies
    /// `DeleteAt <= UpdateAt` with the two equal *only* when no millisecond ticks between the two
    /// clock reads. Measured: a full parity run produced a row 1 ms apart, and an assertion that
    /// the two columns are equal is flaky rather than wrong-headed. Reordering the two lines here
    /// — `delete_at` from its own `get_millis()` — would be invisible almost always and would
    /// change what `DeleteAt` means on the runs where it is not.
    ///
    /// # The seat limit is not checked here
    ///
    /// `isAtUserLimit` and the licensed-seat warning both sit inside `if active`, so neither runs
    /// on this arm. A deactivation therefore needs no licence and cannot be refused by a limit,
    /// which is why this half is served on a licensed server where activation is forwarded.
    ///
    /// # The order of the tail is the security property
    ///
    /// `RevokeAllSessions` comes **before** `userDeactivated`, and inside `userDeactivated` the
    /// OAuth access-data delete comes last. Sessions first means a client cannot spend the
    /// window re-authenticating; see [`mm_store::OAuthStore::remove_all_access_data`] for the
    /// other direction, where Go's comment says why the opposite order is right there.
    /// The last step is the `UserHasBeenDeactivated` plugin hook
    /// ([`App::user_has_been_deactivated`]), after the event and only under the Rust plugin host.
    ///
    /// # Precondition
    ///
    /// The caller has established that `user` owns no bots ([`App::owns_bots`]) — see the module
    /// doc. This function reproduces `userDeactivated`'s bot-free path only.
    #[tracing::instrument(skip_all, fields(user_id = %user.id))]
    pub async fn deactivate_user(&self, ctx: &HookContext, user: &User) -> AppResult<User> {
        let mut user = user.clone();
        user.update_at = mm_model::utils::get_millis();
        user.delete_at = user.update_at;

        let update = self
            .store()
            .user()
            .update(&user, true)
            .await
            .map_err(|err| deactivate_error(err, &user.id))?;
        let new_user = update.new;

        self.revoke_all_sessions(&new_user.id).await?;
        self.user_deactivated(&new_user.id).await;
        self.send_updated_user_event(&new_user).await;
        self.user_has_been_deactivated(ctx, &new_user);
        Ok(new_user)
    }

    /// Port of `App.userDeactivated` (app/user.go:1172), for an owner of no bots.
    ///
    /// **Every step here is best-effort in Go and so it is here.** `userDeactivated` returns an
    /// error only from its `GetUser` re-read; the bot notification, the bot cascade and both
    /// OAuth deletes are wrapped in `rctx.Logger().Warn(…)` and the function continues. A
    /// deactivation whose OAuth sweep fails is still a deactivation, and turning either delete
    /// into a `?` here would make this server refuse a request Go answers 200.
    async fn user_deactivated(&self, user_id: &str) {
        self.set_status_offline(user_id, false, true).await;

        // Go re-reads the user here to ask `IsBot`; the answer only selects the notification,
        // which this path has already established is a no-op. The two OAuth deletes below do not
        // read it.
        if let Err(err) = self
            .store()
            .oauth()
            .remove_auth_data_by_user_id(user_id)
            .await
        {
            tracing::warn!(error = %err, user_id, "unable to remove auth data by user id");
        }
        if let Err(err) = self
            .store()
            .oauth()
            .permanent_delete_auth_data_by_user(user_id)
            .await
        {
            tracing::warn!(error = %err, user_id, "unable to remove oauth access data by user id");
        }
    }
}

/// `App.UpdateActive`'s three store-error shapes (app/user.go:1253), under the same `Where` the
/// activation half uses — Go has one function for both arms and one set of ids.
fn deactivate_error(err: mm_store::StoreError, user_id: &str) -> Box<AppError> {
    match err {
        mm_store::StoreError::Invalid { app_error, .. } => app_error,
        mm_store::StoreError::InvalidInput { .. } => AppError::boxed(
            "UpdateActive",
            "app.user.update.find.app_error",
            None,
            String::new(),
            400,
        ),
        other => {
            tracing::error!(error = %other, user_id, "user deactivation failed");
            AppError::boxed(
                "UpdateActive",
                "app.user.update.finding.app_error",
                None,
                String::new(),
                500,
            )
        }
    }
}

/// `NewAppError("PermanentDeleteUser", id, nil, "", 500).Wrap(err)` — the arm every store call
/// in [`App::permanent_delete_user`] but the bot delete takes. Each call names its own id and
/// they are not interchangeable; the caller passes the one Go gives *that* call.
fn permanent_delete_error(id: &'static str) -> impl FnOnce(StoreError) -> Box<AppError> {
    move |err| {
        tracing::error!(error = %err, id, "permanent user delete failed");
        Box::new(AppError::new("PermanentDeleteUser", id, None, String::new(), 500).wrap(err))
    }
}

/// `app.bot.permenent_delete.bad_id` (sic — Go's spelling) for [`StoreError::InvalidInput`],
/// the only kind `SqlBotStore.PermanentDelete` returns, with the id in `params`; the
/// `internal_error` fallback for anything else (app/user.go:2196).
fn bot_permanent_delete_error(err: StoreError) -> Box<AppError> {
    tracing::error!(error = %err, "permanent user delete failed at the bot row");
    let app_error = match &err {
        StoreError::InvalidInput { value, .. } => AppError::new(
            "PermanentDeleteUser",
            "app.bot.permenent_delete.bad_id",
            Some(HashMap::from([(
                "user_id".to_owned(),
                serde_json::Value::String(value.clone()),
            )])),
            String::new(),
            400,
        ),
        _ => AppError::new(
            "PermanentDeleteUser",
            "app.bot.permanent_delete.internal_error",
            None,
            String::new(),
            500,
        ),
    };
    Box::new(app_error.wrap(err))
}

impl App {
    /// Whether a permanent delete of `user_id` has to be handed to Go whole.
    ///
    /// Two reasons, both decided from reads and both before anything is written, because the
    /// first thing `PermanentDeleteUser` does is the deactivation and there is no prefix of it
    /// that can be forwarded:
    ///
    /// - the account owns a live bot, so `UpdateActive` would reach the sysadmin DM and the bot
    ///   cascade ([`App::owns_bots`], [D-472]);
    /// - the file backend is not the local driver, so the file-store sweep cannot run here.
    #[tracing::instrument(skip(self), fields(user_id = %user_id))]
    pub async fn permanent_delete_needs_go(&self, user_id: &str) -> AppResult<bool> {
        if !self.file_backend().is_supported() {
            return Ok(true);
        }
        self.owns_bots(user_id).await
    }

    /// Port of `App.PermanentDeleteUser` (app/user.go:2134).
    ///
    /// # Precondition
    ///
    /// [`App::permanent_delete_needs_go`] answered `false` for this user.
    ///
    /// # The order, and what each step answers
    ///
    /// `UpdateActive(user, false)` — [`App::deactivate_user`], whose errors pass through — and
    /// then fifteen store calls in Go's order, **each with its own error id**, every one a 500
    /// except the bot row's (see [`bot_permanent_delete_error`]). A failure stops the sequence
    /// where it is: nothing is rolled back, so the rows before it are gone and the ones after it
    /// remain, on both servers alike.
    ///
    /// The file-store half sits between the bot row and the `FileInfo` rows and **never stops
    /// the sequence**: a `GetForUser` failure is a warning and an empty list, each file removal
    /// only logs, and a failure to check or remove `users/<id>/` sets a flag that is answered only
    /// at the very end — as a **202** carrying `app.file_info.permanent_delete_by_user.app_error`,
    /// after the user is already gone. The handler returns it through `c.Err`, so the client sees
    /// an error body with a success-range status.
    ///
    /// # Go's caches
    ///
    /// Go ends with `InvalidateCacheForUser`. The sessions half of that already reached Go through
    /// the deactivation's `RevokeAllSessions` ([`App::clear_session_cache_for_user`]); the profile
    /// half cannot, because the only route that purges Go's cached user
    /// ([`crate::peer_cache::PeerCache::invalidate_user`]) needs the row this has just deleted.
    /// Go may serve the erased profile from its cache until the entry ages out — [D-190].
    #[tracing::instrument(skip_all, fields(user_id = %user.id))]
    pub async fn permanent_delete_user(&self, ctx: &HookContext, user: &User) -> AppResult<()> {
        tracing::warn!(user_id = %user.id, user_email = %user.email, "Attempting to permanently delete account");
        if user.is_in_role(SYSTEM_ADMIN_ROLE_ID) {
            tracing::warn!(user_email = %user.email, "You are deleting a user that is a system administrator.  You may need to set another account as the system administrator using the command line tools.");
        }

        self.deactivate_user(ctx, user).await?;

        let store = self.store();
        let id = user.id.as_str();
        store
            .session()
            .permanent_delete_sessions_by_user(id)
            .await
            .map_err(permanent_delete_error(
                "app.session.permanent_delete_sessions_by_user.app_error",
            ))?;
        store
            .user_access_token()
            .delete_all_for_user(id)
            .await
            .map_err(permanent_delete_error(
                "app.user_access_token.delete.app_error",
            ))?;
        store
            .oauth()
            .permanent_delete_auth_data_by_user(id)
            .await
            .map_err(permanent_delete_error(
                "app.oauth.permanent_delete_auth_data_by_user.app_error",
            ))?;
        store
            .webhook()
            .permanent_delete_incoming_by_user(id)
            .await
            .map_err(permanent_delete_error(
                "app.webhooks.permanent_delete_incoming_by_user.app_error",
            ))?;
        store
            .webhook()
            .permanent_delete_outgoing_by_user(id)
            .await
            .map_err(permanent_delete_error(
                "app.webhooks.permanent_delete_outgoing_by_user.app_error",
            ))?;
        store
            .command()
            .permanent_delete_by_user(id)
            .await
            .map_err(permanent_delete_error(
                "app.user.permanentdeleteuser.internal_error",
            ))?;
        store
            .preference()
            .permanent_delete_by_user(id)
            .await
            .map_err(permanent_delete_error(
                "app.preference.permanent_delete_by_user.app_error",
            ))?;
        store
            .channel()
            .permanent_delete_members_by_user(id)
            .await
            .map_err(permanent_delete_error(
                "app.channel.permanent_delete_members_by_user.app_error",
            ))?;
        store
            .group()
            .permanent_delete_members_by_user(id)
            .await
            .map_err(permanent_delete_error(
                "app.group.permanent_delete_members_by_user.app_error",
            ))?;
        store
            .post()
            .permanent_delete_by_user(id)
            .await
            .map_err(permanent_delete_error(
                "app.post.permanent_delete_by_user.app_error",
            ))?;
        store
            .reaction()
            .permanent_delete_by_user(id)
            .await
            .map_err(permanent_delete_error(
                "app.reaction.permanent_delete_by_user.app_error",
            ))?;
        store
            .scheduled_post()
            .permanent_delete_by_user(id)
            .await
            .map_err(permanent_delete_error(
                "app.scheduled_post.permanent_delete_by_user.app_error",
            ))?;
        store
            .draft()
            .permanent_delete_by_user(id)
            .await
            .map_err(permanent_delete_error(
                "app.drafts.permanent_delete_by_user.app_error",
            ))?;
        store
            .bot()
            .permanent_delete(id)
            .await
            .map_err(bot_permanent_delete_error)?;

        let infos = match store.file_info().get_for_user(id).await {
            Ok(infos) => infos,
            Err(err) => {
                tracing::warn!(error = %err, "Error getting file list for user from FileInfoStore");
                Vec::new()
            }
        };
        self.remove_files_from_file_store(&infos).await;

        // "delete directory containing user's profile image"
        let profile_image_directory = format!("users/{id}");
        let profile_image_path = format!("users/{id}/profile.png");
        let mut file_handling_errors_found = false;
        let exists = match self.file_exists(&profile_image_path).await {
            Ok(exists) => exists,
            Err(err) => {
                file_handling_errors_found = true;
                tracing::warn!(path = %profile_image_path, error = ?err, "Error checking existence of profile image.");
                false
            }
        };
        if exists {
            if let Err(err) = self.remove_directory(&profile_image_directory).await {
                file_handling_errors_found = true;
                tracing::warn!(path = %profile_image_directory, error = ?err, "Unable to remove profile image directory");
            }
        }

        store
            .file_info()
            .permanent_delete_by_user(id)
            .await
            .map_err(permanent_delete_error(
                "app.file_info.permanent_delete_by_user.app_error",
            ))?;
        store
            .user()
            .permanent_delete(id)
            .await
            .map_err(permanent_delete_error(
                "app.user.permanent_delete.app_error",
            ))?;
        store
            .audit()
            .permanent_delete_by_user(id)
            .await
            .map_err(permanent_delete_error(
                "app.audit.permanent_delete_by_user.app_error",
            ))?;
        store
            .team()
            .remove_all_members_by_user(id)
            .await
            .map_err(permanent_delete_error("app.team.remove_member.app_error"))?;

        if file_handling_errors_found {
            return Err(AppError::boxed(
                "PermanentDeleteUser",
                "app.file_info.permanent_delete_by_user.app_error",
                None,
                "Couldn't delete profile image of the user.",
                202,
            ));
        }

        tracing::warn!(user_email = %user.email, user_id = %user.id, "Permanently deleted account");
        Ok(())
    }

    /// Port of `App.RemoveFilesFromFileStore` (app/file.go) over `RemoveFileFromFileStore`.
    ///
    /// For each file: its `Path`, then — errors ignored — its preview and thumbnail when the
    /// info names one. A missing file is a warning and not a failure. Returns the ids of the
    /// infos whose main file could not be removed, which is Go's `[]*AppError` reduced to the one
    /// thing each entry carries (`FileInfoID`); `PermanentDeleteUser` discards it either way.
    pub async fn remove_files_from_file_store(&self, infos: &[FileInfo]) -> Vec<String> {
        let mut failed = Vec::new();
        for info in infos {
            if let RemoveOutcome::Failed = self.remove_file_from_file_store(&info.path).await {
                failed.push(info.id.clone());
            }
            if !info.preview_path.is_empty() {
                self.remove_file_from_file_store(&info.preview_path).await;
            }
            if !info.thumbnail_path.is_empty() {
                self.remove_file_from_file_store(&info.thumbnail_path).await;
            }
        }
        failed
    }

    /// Port of `App.RemoveFileFromFileStore` (app/file.go): check, then remove, logging either
    /// failure. Go's not-found is a 404 `AppError` its one caller filters out; here it is
    /// [`RemoveOutcome::Missing`].
    async fn remove_file_from_file_store(&self, path: &str) -> RemoveOutcome {
        match self.file_exists(path).await {
            Err(err) => {
                tracing::warn!(path, error = ?err, "Error checking existence of file");
                RemoveOutcome::Failed
            }
            Ok(false) => {
                tracing::warn!(path, "File not found");
                RemoveOutcome::Missing
            }
            Ok(true) => match self.remove_file(path).await {
                Ok(()) => RemoveOutcome::Removed,
                Err(err) => {
                    tracing::warn!(path, error = ?err, "Unable to remove file");
                    RemoveOutcome::Failed
                }
            },
        }
    }

    /// `SqlUserStore.GetAll` behind `PermanentDeleteAllUsers`'s one refusal: a 500
    /// `app.user.get.app_error` (app/user.go:2270).
    pub async fn get_all_users(&self) -> AppResult<Vec<User>> {
        self.store().user().get_all().await.map_err(|err| {
            tracing::error!(error = %err, "could not list every user");
            Box::new(
                AppError::new(
                    "PermanentDeleteAllUsers",
                    "app.user.get.app_error",
                    None,
                    String::new(),
                    500,
                )
                .wrap(err),
            )
        })
    }

    /// Whether erasing `users`, in this order, would reach a bot cascade anywhere — so that
    /// `localPermanentDeleteAllUsers` must be forwarded whole.
    ///
    /// # The question is about order, not ownership
    ///
    /// Go erases users in `GetAll`'s order (`Username ASC`, by the database's collation) and asks
    /// `GetBots(OwnerId: u, IncludeDeleted: false)` when it reaches each one. A bot row is removed
    /// when its **own** account is erased (`Bot().PermanentDelete`) and by nothing else on this
    /// path, because the only other writer is the cascade itself. So owner `o` finds bot `b` —
    /// and takes the DM-and-disable path this process cannot reproduce — exactly when `b` is live
    /// now and `b`'s account comes **at or after** `o`'s in the list. "At": a bot that owns
    /// itself finds itself, since its row is deleted later in its own erasure.
    ///
    /// A bot sorted before its owner is erased first and never cascades; forwarding on mere
    /// ownership would hand Go every wipe of a stack that has one, which a fresh stack does
    /// (`seed-bot`, owned by `sliceuser`). Positions are taken from `users` as the database
    /// ordered it, never by comparing strings here.
    ///
    /// An unsupported file backend forwards too, as for one user.
    #[tracing::instrument(skip_all, fields(users = users.len(), cascade))]
    pub async fn permanent_delete_all_needs_go(&self, users: &[User]) -> AppResult<bool> {
        if !self.file_backend().is_supported() {
            return Ok(true);
        }
        let position: HashMap<&str, usize> = users
            .iter()
            .enumerate()
            .map(|(index, user)| (user.id.as_str(), index))
            .collect();

        const PER_PAGE: i32 = 200;
        let mut page = 0;
        loop {
            // `App.GetBots`, the same read — and the same 500 — each per-owner ask in Go makes.
            let bots = self
                .get_bots(&BotGetOptions {
                    owner_id: String::new(),
                    include_deleted: false,
                    only_orphaned: false,
                    page,
                    per_page: PER_PAGE,
                })
                .await?;
            for bot in &bots.0 {
                if let (Some(owner), Some(own)) = (
                    position.get(bot.owner_id.as_str()),
                    position.get(bot.user_id.as_str()),
                ) && own >= owner
                {
                    tracing::Span::current().record("cascade", true);
                    return Ok(true);
                }
            }
            if bots.0.len() < usize::try_from(PER_PAGE).unwrap_or(usize::MAX) {
                break;
            }
            page += 1;
        }
        tracing::Span::current().record("cascade", false);
        Ok(false)
    }

    /// Port of `App.PermanentDeleteAllUsers` (app/user.go:2269), after `GetAll`.
    ///
    /// # Precondition
    ///
    /// [`App::permanent_delete_all_needs_go`] answered `false` for this same list.
    ///
    /// Every failure is a warning and the loop moves on: Go answers `{"status":"OK"}` however
    /// many accounts survived. That includes a user whose erasure stopped half-way — its earlier
    /// tables are gone and its `Users` row is not.
    #[tracing::instrument(skip_all, fields(users = users.len()))]
    pub async fn permanent_delete_users(&self, ctx: &HookContext, users: &[User]) {
        for user in users {
            if let Err(err) = self.permanent_delete_user(ctx, user).await {
                tracing::warn!(user_id = %user.id, error = %err.id, "Error while deleting user");
            }
        }
    }
}

/// What one `RemoveFileFromFileStore` came to.
enum RemoveOutcome {
    Removed,
    Missing,
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bot row's two arms (app/user.go:2196): `ErrInvalidInput` — the only kind
    /// `SqlBotStore.PermanentDelete` returns — is a **400** with the id in `params` under Go's
    /// misspelt `permenent`; anything else is the 500 fallback. Transcribed from the Go source: no
    /// route can make the `DELETE FROM Bots` fail on a live database.
    #[test]
    fn the_bot_row_maps_invalid_input_to_a_400_naming_the_id() {
        let invalid = bot_permanent_delete_error(StoreError::InvalidInput {
            entity: "Bot",
            field: "UserId",
            value: "someid".to_owned(),
        });
        assert_eq!(invalid.id, "app.bot.permenent_delete.bad_id");
        assert_eq!(invalid.status_code, 400);
        assert_eq!(invalid.where_, "PermanentDeleteUser");
        assert_eq!(
            invalid.params.as_ref().and_then(|p| p.get("user_id")),
            Some(&serde_json::Value::String("someid".to_owned()))
        );

        let other = bot_permanent_delete_error(StoreError::NotFound {
            entity: "Bot",
            criteria: "someid".to_owned(),
        });
        assert_eq!(other.id, "app.bot.permanent_delete.internal_error");
        assert_eq!(other.status_code, 500);
        assert!(other.params.is_none());
    }

    /// Every other store call is a 500 under `PermanentDeleteUser` with the id it is given —
    /// whatever kind of error the store returned, invalid input included.
    #[test]
    fn every_other_store_call_is_a_500_with_its_own_id() {
        let err = permanent_delete_error("app.drafts.permanent_delete_by_user.app_error")(
            StoreError::InvalidInput {
                entity: "Draft",
                field: "UserId",
                value: String::new(),
            },
        );
        assert_eq!(err.id, "app.drafts.permanent_delete_by_user.app_error");
        assert_eq!(err.status_code, 500);
        assert_eq!(err.where_, "PermanentDeleteUser");
    }
}
