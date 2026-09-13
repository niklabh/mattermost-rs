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
//! - The `UserHasBeenDeactivated` plugin hook, in `Srv().Go(…)` after the response is written.
//!   There is no plugin host here and none installed on the stack this is tested against, so the
//!   hook fires over an empty list on both sides. See [D-471].
//! - `PermanentDeleteUser`, the `?permanent=true` arm — eighteen store families, unreachable
//!   while `ServiceSettings.EnableAPIUserDeletion` is off, and forwarded. See [D-470].

use mm_model::bot::BotGetOptions;
use mm_model::user::User;
use mm_model::utils::{AppError, AppResult};
use mm_store::{OAuthStore, UserStore};

use crate::App;

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
    ///
    /// # Precondition
    ///
    /// The caller has established that `user` owns no bots ([`App::owns_bots`]) — see the module
    /// doc. This function reproduces `userDeactivated`'s bot-free path only.
    #[tracing::instrument(skip_all, fields(user_id = %user.id))]
    pub async fn deactivate_user(&self, user: &User) -> AppResult<User> {
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
