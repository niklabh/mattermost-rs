//! Port of the `App` half of `channels/app/bot.go` — the two reads and the four writes.
//!
//! Reads: `GetBot` (:333) and `GetBots` (:348). Writes: `CreateBot` (:95), `PatchBot` (:263),
//! `UpdateBotActive` (:396) and `UpdateBotOwner` (:471). The permission gate the last three
//! share, `SessionHasPermissionToManageBot`, was already ported for
//! `SessionHasPermissionToUserOrBot` and lives in [`crate::authorization`].
//!
//! Two calls, and the only content is the error split — which is the same split with a different
//! answer on each side:
//!
//! | | miss | anything else |
//! |---|---|---|
//! | `GetBot` | `MakeBotNotFoundError` — **404**, `store.sql_bot.get.missing.app_error`, and it carries a `user_id` param | `app.bot.getbot.internal_error`, 500 |
//! | `GetBots` | — a query matching nothing is an empty list | `app.bot.getbots.internal_error`, 500 |
//!
//! The 404's id is a **store** id reached through the app layer, and its `where` is
//! `SqlBotStore.Get` rather than `GetBot`. Both are on the wire, so both are reproduced verbatim.

use mm_model::bot::{
    BOT_SYSTEM_BOT_USERNAME, Bot, BotGetOptions, BotList, BotPatch, make_bot_not_found_error,
    user_from_bot,
};
use mm_model::user::User;
use mm_model::utils::{AppError, AppResult, get_millis};
use mm_store::{BotStore, StoreError, UserStore};

use crate::App;
use crate::plugin_hooks::HookContext;

/// `app.MissingAccountError` (channels/app/constants.go:7).
const MISSING_ACCOUNT_ERROR: &str = "app.user.missing_account.const";

/// The decision in [`App::is_bot_exempt_from_dm_restrictions`] once the bot is loaded, without
/// a database. `available_plugin_ids` is only called when the rule gets that far and this process
/// hosts the plugins.
pub(crate) fn bot_exemption(
    bot: &Bot,
    session_user_id: Option<&str>,
    plugins_hosted_here: bool,
    plugins_enabled: bool,
    available_plugin_ids: impl FnOnce() -> AppResult<Vec<String>>,
) -> AppResult<BotExemption> {
    if bot.username == BOT_SYSTEM_BOT_USERNAME {
        return Ok(BotExemption::Exempt);
    }
    if session_user_id.is_some_and(|user_id| user_id == bot.owner_id) {
        return Ok(BotExemption::Exempt);
    }
    if !plugins_hosted_here {
        return Ok(if plugins_enabled {
            BotExemption::Undecidable
        } else {
            BotExemption::NotExempt
        });
    }
    let ids = available_plugin_ids()?;
    Ok(if ids.contains(&bot.owner_id) {
        BotExemption::Exempt
    } else {
        BotExemption::NotExempt
    })
}

/// What [`App::is_bot_exempt_from_dm_restrictions`] could determine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BotExemption {
    Exempt,
    NotExempt,
    /// The answer is "is the owner a plugin in Go's plugin directory", and Go hosts the plugins
    /// (`MMRS_PLUGIN_HOST=go`) with plugins enabled. The caller forwards.
    Undecidable,
}

impl App {
    /// Port of `App.IsBotExemptFromDMRestrictions` (bot.go:359) — whether a bot may DM across
    /// `TeamSettings.RestrictDirectMessage = "team"`.
    ///
    /// In Go's order: `GetBot` (a deactivated bot is its **404**, which the caller returns), the
    /// system bot by username, a bot the **session's** user owns, then "is the owner the id of an
    /// available plugin" — every bundle in the plugin directory, running or not.
    ///
    /// # The plugin half depends on which process hosts plugins
    ///
    /// Under `MMRS_PLUGIN_HOST=rust` the environment is ours and the rule is ported whole: no
    /// environment (plugins off, or not started) is `NotExempt`, a directory that cannot be listed
    /// is Go's 500. Under the Go host, Go's environment is nil exactly when `PluginSettings.Enable`
    /// is off — the one config both processes read — so that case is decided here too; with
    /// plugins on, the directory is Go's, and the answer is [`BotExemption::Undecidable`].
    ///
    /// `session_user_id` is `None` where Go's context has no user (the plugin API): its
    /// `session.UserId` is `""`, which owns no bot.
    #[tracing::instrument(skip(self), fields(user_id = %user_id))]
    pub async fn is_bot_exempt_from_dm_restrictions(
        &self,
        session_user_id: Option<&str>,
        user_id: &str,
    ) -> AppResult<BotExemption> {
        let bot = self.get_bot(user_id, false).await?;
        bot_exemption(
            &bot,
            session_user_id,
            self.plugin_host().hosted(),
            self.config().plugin_enable,
            || match self.plugins_environment() {
                None => Ok(Vec::new()),
                Some(environment) => environment
                    .available()
                    .map(|bundles| {
                        bundles
                            .into_iter()
                            .filter_map(|bundle| bundle.manifest.map(|manifest| manifest.id))
                            .collect()
                    })
                    .map_err(|err| {
                        tracing::error!(error = %err, "listing the available plugins failed");
                        AppError::boxed(
                            "IsBotExemptFromDMRestrictions",
                            "app.plugin.get_plugins.app_error",
                            None,
                            String::new(),
                            500,
                        )
                    }),
            },
        )
    }

    /// Port of `App.GetBot` (bot.go:333).
    ///
    /// **The not-found error is the same one the handler raises for a permission failure.** Go's
    /// comment says why: "the errors must be the same in both cases to avoid leaking that a user
    /// is a bot". So a caller who may not read this bot and a caller asking about an id that does
    /// not exist get byte-identical answers — see `mm_api::bots::get_bot`, which depends on it.
    #[tracing::instrument(skip(self), fields(bot_user_id = %bot_user_id, include_deleted))]
    pub async fn get_bot(&self, bot_user_id: &str, include_deleted: bool) -> AppResult<Bot> {
        self.store()
            .bot()
            .get(bot_user_id, include_deleted)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    // Go passes `nfErr.ID` — the id the store put in its `ErrNotFound`, which is
                    // the bot user id it was asked for.
                    return make_bot_not_found_error("SqlBotStore.Get", bot_user_id);
                }
                tracing::error!(error = ?err, "bot lookup failed");
                AppError::boxed(
                    "GetBot",
                    "app.bot.getbot.internal_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `App.GetBots` (bot.go:348). One error, always a 500.
    #[tracing::instrument(skip_all, fields(owner_id = %options.owner_id, found))]
    pub async fn get_bots(&self, options: &BotGetOptions) -> AppResult<BotList> {
        let bots = self
            .store()
            .bot()
            .get_all(options)
            .await
            .map_err(get_bots_error)?;
        tracing::Span::current().record("found", bots.0.len());
        Ok(bots)
    }
}

fn get_bots_error(err: StoreError) -> Box<AppError> {
    tracing::error!(error = ?err, "bot list lookup failed");
    AppError::boxed(
        "GetBots",
        "app.bot.getbots.internal_error",
        None,
        String::new(),
        500,
    )
}

// =================================================================================================
// The write half: `createBot`, `patchBot`, `updateBotActive` and `assignBot`.
// =================================================================================================

impl App {
    /// Port of `App.CreateBot` (app/bot.go:95) — behind `POST /api/v4/bots`.
    ///
    /// # Two tables, and the rollback between them
    ///
    /// The `Users` row goes in first, because [`mm_store::BotStore::get`] inner-joins it and a
    /// `Bots` row without one is invisible to every read route. If the bot insert then fails, Go
    /// deletes the user it just created and **logs** the delete's own failure rather than
    /// reporting it — so a half-created bot is possible on both servers, and the client is told
    /// about the insert that failed, never about the cleanup that also did.
    ///
    /// # The user id is generated by `PreSave`, not by this function
    ///
    /// `UserFromBot` copies `Bot.UserId` into `User.Id`, which is empty for a create, so the
    /// store mints it. `bot.UserId = user.Id` afterwards is what ties the two rows together.
    ///
    /// # Not reproduced: the DM to the owner
    ///
    /// Go finishes by opening a direct channel with the owner and posting
    /// `api.bot.teams_channels.add_message_mobile` into it as the bot, through `CreatePostAsUser`
    /// — a function this server does not have (`POST /posts` is not migrated). The owner lookup
    /// **is** kept, because its non-`NotFound` branch is a wire-visible 500 and because it is
    /// where the divergence begins. Two consequences, both recorded as [D-281]: a bot created
    /// through this server leaves the owner no DM, and Go's create can still fail *after* both
    /// rows are written where ours cannot.
    #[tracing::instrument(skip_all, fields(username = %bot.username, owner_id = %bot.owner_id, bot_user_id))]
    pub async fn create_bot(&self, bot: &Bot) -> AppResult<Bot> {
        // `IsValidCreate`, not `IsValid`: `UserId`, `CreateAt` and `UpdateAt` are filled in by
        // `PreSave` further down and must not be checked yet.
        bot.is_valid_create()?;

        // Go mutates the caller's `*model.Bot` in place (`bot.UserId = user.Id`) and hands the
        // same pointer to the store. Taking `&Bot` and cloning gives the caller the same result
        // without the shared mutation — not a borrow-checker workaround.
        let mut bot = bot.clone();
        let user = self
            .store()
            .user()
            .save(&user_from_bot(&bot), &crate::password::latest_hasher())
            .await
            .map_err(create_bot_user_save_error)?;

        bot.user_id = user.id.clone();
        tracing::Span::current().record("bot_user_id", &bot.user_id);

        let saved_bot = match self.store().bot().save(&bot).await {
            Ok(saved_bot) => saved_bot,
            Err(err) => {
                // Go logs this and carries on to report the *save* failure. A cleanup that fails
                // must not replace the error the client is waiting for.
                if let Err(cleanup) = self.store().user().permanent_delete(&bot.user_id).await {
                    tracing::error!(
                        error = %cleanup,
                        "Failed to permanently delete the user after bot save failure",
                    );
                }
                return Err(match err {
                    StoreError::Invalid { app_error, .. } => app_error,
                    other => {
                        tracing::error!(error = %other, "bot save failed");
                        AppError::boxed(
                            "CreateBot",
                            "app.bot.createbot.internal_error",
                            None,
                            String::new(),
                            500,
                        )
                    }
                });
            }
        };

        // The owner may be a **plugin id** rather than a user, in which case this misses and Go
        // simply sends no message. Only a broken query is an error.
        match self.store().user().get(&bot.owner_id).await {
            Ok(_owner) => {
                // See the module note: the direct channel and the bot's own post are [D-281].
                tracing::debug!(
                    owner_id = %bot.owner_id,
                    "bot created; the owner DM is not ported (D-281)",
                );
            }
            Err(err) if err.is_not_found() => {}
            Err(err) => {
                tracing::error!(error = %err, "the bot owner lookup failed");
                return Err(AppError::boxed(
                    "CreateBot",
                    "app.user.get.app_error",
                    None,
                    String::new(),
                    500,
                ));
            }
        }

        Ok(saved_bot)
    }

    /// Port of `App.PatchBot` (app/bot.go:263) — behind `PUT /api/v4/bots/{bot_user_id}`.
    ///
    /// # A patch that changes nothing is a 200 and no write at all
    ///
    /// `WouldPatch` compares each present field against the current value, so `{"username": "x"}`
    /// on a bot already called `x` returns the bot untouched — no `UpdateAt` bump, no
    /// `user_updated` event, no rows written. A port that skipped the guard would tick `UpdateAt`
    /// on every call and break every client etag.
    ///
    /// # The `Users` row is written **before** the `Bots` row, and that ordering is on the wire
    ///
    /// [`mm_store::BotStore::update`] re-reads the join and returns what it read, so the username
    /// in the answer is whatever `Users` holds at that moment. Writing the bot first would answer
    /// with the *old* username while having stored the new one.
    ///
    /// Only four user columns follow the patch — `Id`, `Username`, `Email`, `FirstName`. The
    /// email is the generated `<username>@localhost`, so renaming a bot silently rewrites its
    /// address; `FirstName` is where a bot's `display_name` lives.
    #[tracing::instrument(skip_all, fields(bot_user_id = %bot_user_id, would_patch))]
    pub async fn patch_bot(&self, bot_user_id: &str, patch: &BotPatch) -> AppResult<Bot> {
        let mut bot = self.get_bot(bot_user_id, true).await?;

        let would_patch = bot.would_patch(Some(patch));
        tracing::Span::current().record("would_patch", would_patch);
        if !would_patch {
            return Ok(bot);
        }

        bot.patch(patch);

        let mut user = self.store().user().get(bot_user_id).await.map_err(|err| {
            if err.is_not_found() {
                return AppError::boxed(
                    "PatchBot",
                    MISSING_ACCOUNT_ERROR,
                    None,
                    String::new(),
                    404,
                );
            }
            tracing::error!(error = %err, "the bot's user row could not be read");
            AppError::boxed(
                "PatchBot",
                "app.user.get.app_error",
                None,
                String::new(),
                500,
            )
        })?;

        let patched_user = user_from_bot(&bot);
        user.id = patched_user.id;
        user.username = patched_user.username;
        user.email = patched_user.email;
        user.first_name = patched_user.first_name;

        // `trustedUpdateData = true` — the one api4-reachable caller that passes it. Without it
        // the store would restore `Roles` and `DeleteAt` from the stored row, which is harmless
        // here, but it would also refuse an LDAP user's rename; a bot is neither.
        let update = self
            .store()
            .user()
            .update(&user, true)
            .await
            .map_err(|err| patch_bot_user_update_error(err, bot_user_id))?;

        self.send_updated_user_event(&update.new).await;

        self.store().bot().update(&bot).await.map_err(|err| {
            bot_update_error(
                "PatchBot",
                "app.bot.patchbot.internal_error",
                err,
                bot_user_id,
            )
        })
    }

    /// Port of `App.UpdateBotActive` (app/bot.go:396) — behind `POST /bots/{id}/disable` and
    /// `/enable`.
    ///
    /// # There is no `enabled` column
    ///
    /// Disabling a bot is a soft delete on **two** rows: `Users.DeleteAt` through
    /// [`App::update_active_for_bot`], then `Bots.DeleteAt`. Enabling clears both. The bot half
    /// is guarded by `changed`, so enabling an already-enabled bot writes no `Bots` row and does
    /// not bump its `UpdateAt` — but the *user* half above runs unconditionally and does bump
    /// `Users.UpdateAt` either way. Two rows, two different idempotency rules, and only one of
    /// them is visible in this route's answer.
    ///
    /// # `Where` says `PatchBot` on three of its five errors
    ///
    /// Go's copy-paste, reproduced: the missing-account 404, the user-read 500 and the final bot
    /// update 500 all name `PatchBot`. `where` is not serialised, so this costs a client nothing
    /// — but changing it would be inventing a difference rather than removing one.
    #[tracing::instrument(skip_all, fields(bot_user_id = %bot_user_id, active, changed))]
    pub async fn update_bot_active(
        &self,
        ctx: &HookContext,
        bot_user_id: &str,
        active: bool,
    ) -> AppResult<Bot> {
        let user = self.store().user().get(bot_user_id).await.map_err(|err| {
            if err.is_not_found() {
                return AppError::boxed(
                    "PatchBot",
                    MISSING_ACCOUNT_ERROR,
                    None,
                    String::new(),
                    404,
                );
            }
            tracing::error!(error = %err, "the bot's user row could not be read");
            AppError::boxed(
                "PatchBot",
                "app.user.get.app_error",
                None,
                String::new(),
                500,
            )
        })?;

        self.update_active_for_bot(ctx, &user, active).await?;

        let mut bot = self
            .store()
            .bot()
            .get(bot_user_id, true)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    return make_bot_not_found_error("SqlBotStore.Get", bot_user_id);
                }
                tracing::error!(error = %err, "bot lookup failed");
                AppError::boxed(
                    "UpdateBotActive",
                    "app.bot.getbot.internal_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        // Go's three-armed `if`: the two arms that change something, and an `else` that does not.
        // Note the asymmetry — enabling tests `DeleteAt != 0` and disabling tests `== 0`, so the
        // "already in that state" case falls through on both sides.
        let changed = if active && bot.delete_at != 0 {
            bot.delete_at = 0;
            true
        } else if !active && bot.delete_at == 0 {
            bot.delete_at = get_millis();
            true
        } else {
            false
        };
        tracing::Span::current().record("changed", changed);

        if changed {
            bot = self.store().bot().update(&bot).await.map_err(|err| {
                bot_update_error(
                    "PatchBot",
                    "app.bot.patchbot.internal_error",
                    err,
                    bot_user_id,
                )
            })?;
        }

        Ok(bot)
    }

    /// Port of `App.UpdateBotOwner` (app/bot.go:471) — behind
    /// `POST /bots/{bot_user_id}/assign/{user_id}`.
    ///
    /// The smallest of the four writes: read the bot including deleted ones, overwrite `OwnerId`,
    /// write it back. **Nothing validates that the new owner exists** — `IsValid` only bounds the
    /// id's length, because an owner may legitimately be a plugin id rather than a user. The
    /// handler's own `IsBot` check is the only gate, and it too passes when the id matches no one.
    #[tracing::instrument(skip_all, fields(bot_user_id = %bot_user_id, new_owner_id = %new_owner_id))]
    pub async fn update_bot_owner(&self, bot_user_id: &str, new_owner_id: &str) -> AppResult<Bot> {
        let mut bot = self
            .store()
            .bot()
            .get(bot_user_id, true)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    return make_bot_not_found_error("SqlBotStore.Get", bot_user_id);
                }
                tracing::error!(error = %err, "bot lookup failed");
                AppError::boxed(
                    "UpdateBotOwner",
                    "app.bot.getbot.internal_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        bot.owner_id = new_owner_id.to_owned();

        self.store().bot().update(&bot).await.map_err(|err| {
            bot_update_error(
                "PatchBot",
                "app.bot.patchbot.internal_error",
                err,
                bot_user_id,
            )
        })
    }

    /// The slice of `App.UpdateActive` (app/user.go:1229) that `UpdateBotActive` reaches.
    ///
    /// # The user limit gates **activation only**
    ///
    /// `isAtUserLimit` runs before anything else and only when `active` is true, so a server at
    /// its hard limit can still disable bots. The two ids differ by whether a licence is
    /// installed, and both are 400 — so a client sees `…license_user_limit.exceeded` or
    /// `…user_limit.exceeded` and never a 402 or 403.
    ///
    /// # `DeleteAt` is `UpdateAt`, not a second call to the clock
    ///
    /// `user.UpdateAt = GetMillis()` then `user.DeleteAt = user.UpdateAt`. Reading the clock
    /// twice would leave the two columns a millisecond apart on an unlucky run — and **no test
    /// can see that**: the mutation doing it survives the suite, because the two readings fall in
    /// the same millisecond on all but the unluckiest run and Go would drift the same way. Kept
    /// faithful because it is free to be faithful, not because anything checks.
    ///
    /// # Not reproduced
    ///
    /// `userDeactivated` — offline status, the sysadmin notice, the `disableUserBots` cascade and
    /// the two OAuth auth-data deletions. None is visible in this route's answer; the cascade is
    /// the one with teeth, and it is [D-282]. The `UserHasBeenDeactivated` plugin hook *is*
    /// reproduced: a bot is a user, and disabling one tells every plugin so.
    /// `InvalidateCacheForUser` and `invalidateUserChannelMembersCaches` are in-process caches
    /// this server does not have.
    #[tracing::instrument(skip_all, fields(user_id = %user.id, active))]
    async fn update_active_for_bot(
        &self,
        ctx: &HookContext,
        user: &User,
        active: bool,
    ) -> AppResult {
        if active {
            let limits = self.get_server_limits(true).await?;
            // "Zero means no limit" — a comparison without this guard would refuse every
            // activation on a server that has never configured one.
            let at_limit = limits.max_users_hard_limit != 0
                && limits.active_user_count >= limits.max_users_hard_limit;
            if at_limit {
                let id = if self.license().await?.is_some() {
                    "app.user.update_active.license_user_limit.exceeded"
                } else {
                    "app.user.update_active.user_limit.exceeded"
                };
                return Err(AppError::boxed(
                    "UpdateActive",
                    id,
                    None,
                    String::new(),
                    400,
                ));
            }
        }

        // Go writes `UpdateAt` and `DeleteAt` onto the caller's `*model.User`, which
        // `UpdateBotActive` then stops using. Cloning keeps that mutation local.
        let mut user = user.clone();
        user.update_at = get_millis();
        user.delete_at = if active { 0 } else { user.update_at };

        let update = self
            .store()
            .user()
            .update(&user, true)
            .await
            .map_err(update_active_error)?;

        if !active {
            self.revoke_all_sessions(&update.new.id).await?;
        }

        self.send_updated_user_event(&update.new).await;
        if !active {
            self.user_has_been_deactivated(ctx, &update.new);
        }
        Ok(())
    }
}

/// `App.CreateBot`'s four shapes for a failed `User().Save` (app/bot.go:100).
///
/// The `ErrInvalidInput` arm switches on the **field**, and the default id is neither of the
/// other two: `app.user.save.existing.app_error`, which is what a violation of some *other*
/// unique constraint would report.
fn create_bot_user_save_error(err: StoreError) -> Box<AppError> {
    match err {
        // `errors.As(nErr, &appErr)` — `PreSave`/`IsValid` keep their own id and status, so a bot
        // whose generated email is somehow invalid reports `model.user.is_valid.email.app_error`
        // and not a create error at all.
        StoreError::Invalid { app_error, .. } => app_error,
        StoreError::InvalidInput { field, .. } => {
            let id = match field {
                "email" => "app.user.save.email_exists.app_error",
                "username" => "app.user.save.username_exists.app_error",
                _ => "app.user.save.existing.app_error",
            };
            AppError::boxed("CreateBot", id, None, String::new(), 400)
        }
        other => {
            tracing::error!(error = %other, "the bot's user row could not be saved");
            AppError::boxed(
                "CreateBot",
                "app.user.save.app_error",
                None,
                String::new(),
                500,
            )
        }
    }
}

/// `App.PatchBot`'s five shapes for a failed `User().Update` (app/bot.go:291).
///
/// Same five as `App.UpdateUser`'s, with `PatchBot` as the `where` — except that the two conflict
/// ids here are `app.user.save.*_exists`, i.e. the **save** spellings, on an update path.
fn patch_bot_user_update_error(err: StoreError, bot_user_id: &str) -> Box<AppError> {
    match err {
        StoreError::Invalid { app_error, .. } => app_error,
        StoreError::InvalidInput { .. } => AppError::boxed(
            "PatchBot",
            "app.user.update.find.app_error",
            None,
            String::new(),
            400,
        ),
        StoreError::Conflict { resource, .. } => AppError::boxed(
            "PatchBot",
            if resource == "Username" {
                "app.user.save.username_exists.app_error"
            } else {
                "app.user.save.email_exists.app_error"
            },
            None,
            String::new(),
            400,
        ),
        other => {
            tracing::error!(error = %other, bot_user_id, "the bot's user row could not be updated");
            AppError::boxed(
                "PatchBot",
                "app.user.update.finding.app_error",
                None,
                String::new(),
                500,
            )
        }
    }
}

/// `App.UpdateActive`'s three shapes for a failed user update (app/user.go:1252).
fn update_active_error(err: StoreError) -> Box<AppError> {
    match err {
        StoreError::Invalid { app_error, .. } => app_error,
        StoreError::InvalidInput { .. } => AppError::boxed(
            "UpdateActive",
            "app.user.update.find.app_error",
            None,
            String::new(),
            400,
        ),
        other => {
            tracing::error!(error = %other, "the bot's user row could not be deactivated");
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

/// The three-armed error every `Bot().Update` call site shares (app/bot.go:315, 432, 489).
///
/// The `NotFound` arm is the security-relevant one: it rebuilds the **read** 404, `where` and
/// all, so a bot deleted between the permission check and the write is reported as one that never
/// existed rather than as a write failure.
fn bot_update_error(
    where_: &'static str,
    id: &'static str,
    err: StoreError,
    bot_user_id: &str,
) -> Box<AppError> {
    match err {
        StoreError::NotFound { .. } => make_bot_not_found_error("SqlBotStore.Get", bot_user_id),
        StoreError::Invalid { app_error, .. } => app_error,
        other => {
            tracing::error!(error = %other, bot_user_id, "the bot row could not be updated");
            AppError::boxed(where_, id, None, String::new(), 500)
        }
    }
}

/// The system bot: the actor Go substitutes when a channel write has no user behind it.
///
/// Every `*_local.go` handler that archives, restores or converts a channel, or removes a
/// member, passes an empty user id — and the app layer then posts the system message **as the
/// system bot**, creating it on first use. Before this port that branch was a log line, which
/// left the socket's answer right and the channel's history wrong.
impl App {
    /// Port of `App.GetSystemBot` (bot.go:640).
    ///
    /// `i18n.T("app.system.system_bot.bot_displayname")` is `"System"` in the server's only
    /// bundled locale; the username is `model.BotSystemBotUsername`.
    #[tracing::instrument(skip(self))]
    pub async fn get_system_bot(&self) -> AppResult<Bot> {
        self.get_or_create_system_owned_bot(mm_model::bot::BOT_SYSTEM_BOT_USERNAME, "System")
            .await
    }

    /// Port of `App.GetOrCreateSystemOwnedBot` (bot.go:644).
    ///
    /// The owner is the **first system administrator by username** — `GetUsersFromProfiles`
    /// with `Role: system_admin`, page 0 of size 1, and neither `Inactive` nor `Active` set, so a
    /// deactivated administrator qualifies. No administrator at all is the 500
    /// `app.bot.get_system_bot.empty_admin_list.app_error`, which a real installation cannot
    /// produce (the first user is one) and this port does not special-case.
    #[tracing::instrument(skip(self), fields(username = %username))]
    pub async fn get_or_create_system_owned_bot(
        &self,
        username: &str,
        display_name: &str,
    ) -> AppResult<Bot> {
        // `applyRoleFilter`: `%` + sanitizeSearchTerm(role, "\\") + `%`, compared with `LIKE
        // LOWER(?)`. The role id carries an underscore, which the sanitiser escapes.
        let pattern = format!(
            "%{}%",
            mm_store::user_store::sanitize_search_term(mm_model::role::SYSTEM_ADMIN_ROLE_ID, '\\')
        );
        let admins = self
            .store()
            .user()
            .get_all_profiles_in_role(&pattern, 0, 1)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "the administrator lookup failed");
                AppError::boxed(
                    "GetUsersFromProfiles",
                    "app.user.get_profiles.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        let Some(owner) = admins.first() else {
            return Err(AppError::boxed(
                "GetSystemBot",
                "app.bot.get_system_bot.empty_admin_list.app_error",
                None,
                String::new(),
                500,
            ));
        };

        let definition = Bot {
            username: username.to_owned(),
            display_name: display_name.to_owned(),
            description: String::new(),
            owner_id: owner.id.clone(),
            ..Bot::default()
        };
        self.get_or_create_bot(definition).await
    }

    /// Port of `App.getOrCreateBot` (bot.go:669).
    ///
    /// Looked up **by username**, not by the `Bots` table: an existing user of that name that
    /// is not a bot makes the trailing `GetBot` fail with the bot-not-found error, exactly as in
    /// Go. The create path is `CreateBot`'s first half — the user row, then the bot row, the
    /// user removed again if the bot save fails — without the owner DM and its welcome post.
    #[tracing::instrument(skip_all, fields(username = %definition.username, created))]
    async fn get_or_create_bot(&self, mut definition: Bot) -> AppResult<Bot> {
        match self.get_user_by_username(&definition.username).await {
            Ok(bot_user) => {
                tracing::Span::current().record("created", false);
                self.get_bot(&bot_user.id, false).await
            }
            Err(err) if err.status_code == 404 => {
                tracing::Span::current().record("created", true);
                let user = self
                    .store()
                    .user()
                    .save(
                        &user_from_bot(&definition),
                        &crate::password::latest_hasher(),
                    )
                    .await
                    .map_err(|err| {
                        let mut err = create_bot_user_save_error(err);
                        err.where_ = "getOrCreateBot".to_owned();
                        err
                    })?;
                definition.user_id = user.id;

                match self.store().bot().save(&definition).await {
                    Ok(saved) => Ok(saved),
                    Err(err) => {
                        if let Err(cleanup) = self
                            .store()
                            .user()
                            .permanent_delete(&definition.user_id)
                            .await
                        {
                            tracing::error!(
                                error = %cleanup,
                                "Failed to permanently delete the user after bot save failure",
                            );
                        }
                        Err(match err {
                            StoreError::Invalid { app_error, .. } => app_error,
                            other => {
                                tracing::error!(error = %other, "system bot save failed");
                                AppError::boxed(
                                    "getOrCreateBot",
                                    "app.bot.createbot.internal_error",
                                    None,
                                    String::new(),
                                    500,
                                )
                            }
                        })
                    }
                }
            }
            Err(err) => Err(err),
        }
    }
}

/// `botUserKey` (app/bot.go:21): `internalKeyPrefix + "botid"`, the plugin KV key under which
/// [`App::ensure_bot`] remembers the bot it made.
pub const BOT_USER_KEY: &str = "mmi_botid";

/// What `App.EnsureBot` fails with. It returns an `error`, not an `*AppError`, and three shapes
/// reach the plugin differently: `encodableError` sends an `*AppError` whole and anything else as
/// its `Error()` text.
#[derive(Debug, thiserror::Error)]
pub enum EnsureBotError {
    /// `errors.New` or `fmt.Errorf` with no app error inside: the text is the whole error.
    #[error("{0}")]
    Message(String),
    /// An `*AppError` returned as it is — the KV read's.
    #[error(transparent)]
    App(Box<AppError>),
    /// `fmt.Errorf("<prefix>: %w", appErr)`. The text is the prefix and the app error's
    /// `Error()`, which carries its **translated** message, so the caller renders it once it has
    /// translated the error.
    #[error("{0}: {1}")]
    Wrapped(&'static str, Box<AppError>),
}

impl App {
    /// Port of `App.EnsureBot` (app/bot.go:26), behind the plugin API's `EnsureBotUser` — what the
    /// SDK's `BotService.EnsureBot` calls under its cluster mutex.
    ///
    /// # Three ways to an id, in Go's order
    ///
    /// 1. The id stored under [`BOT_USER_KEY`], if that bot still exists (deleted or not —
    ///    `GetBot(..., true)`): it is **patched** to the requested username, display name and
    ///    description, and its id is the answer. A stored id whose bot is gone is only logged.
    /// 2. An existing user with the requested username: a bot is adopted (its id stored); a
    ///    human is refused, because converting one is an administrator's decision.
    /// 3. A new bot, whose id is then stored.
    ///
    /// The owner is whatever the caller set — the plugin API sets the plugin's id first.
    #[tracing::instrument(skip(self, bot), fields(username = bot.map(|b| b.username.as_str())))]
    pub async fn ensure_bot(
        &self,
        plugin_id: &str,
        bot: Option<&Bot>,
    ) -> Result<String, EnsureBotError> {
        let Some(bot) = bot else {
            return Err(EnsureBotError::Message("passed a nil bot".to_owned()));
        };
        if bot.username.is_empty() {
            return Err(EnsureBotError::Message(
                "passed a bot with no username".to_owned(),
            ));
        }

        let stored = self
            .get_plugin_key(plugin_id, BOT_USER_KEY)
            .await
            .map_err(EnsureBotError::App)?;

        // "If the bot has already been created, check whether it still exists and use it."
        if let Some(bytes) = stored {
            let bot_id = String::from_utf8_lossy(&bytes).into_owned();
            match self.get_bot(&bot_id, true).await {
                Err(err) => {
                    tracing::debug!(bot_id, error = %err, "Unable to get bot.");
                }
                Ok(_) => {
                    // "ensure existing bot is synced with what is being created"
                    let patch = BotPatch {
                        username: Some(bot.username.clone()),
                        display_name: Some(bot.display_name.clone()),
                        description: Some(bot.description.clone()),
                    };
                    self.patch_bot(&bot_id, &patch)
                        .await
                        .map_err(|err| EnsureBotError::Wrapped("failed to patch bot", err))?;
                    return Ok(bot_id);
                }
            }
        }

        // "Check for an existing bot user with that username. If one exists, then use that."
        if let Ok(user) = self.get_user_by_username(&bot.username).await {
            if user.is_bot {
                self.set_plugin_key(plugin_id, BOT_USER_KEY, Some(user.id.as_bytes()))
                    .await
                    .map_err(|err| EnsureBotError::Wrapped("failed to set plugin key", err))?;
                return Ok(user.id);
            }
            tracing::error!(
                username = %bot.username,
                user_id = %user.id,
                "Plugin attempted to use an account that already exists. Convert user to a bot \
                 account in the CLI by running 'mattermost user convert <username> --bot'. If the \
                 user is an existing user account you want to preserve, change its username and \
                 restart the Mattermost server, after which the plugin will create a bot account \
                 with that name. For more information about bot accounts, see \
                 https://mattermost.com/pl/default-bot-accounts",
            );
            // `%q` of a username: a valid username is lowercase ASCII letters, digits and `.-_`,
            // none of which Go's quoting escapes — and only a stored, valid one reaches here.
            return Err(EnsureBotError::Message(format!(
                "username \"{}\" is already taken by a non-bot user",
                bot.username
            )));
        }

        let created = self
            .create_bot(bot)
            .await
            .map_err(|err| EnsureBotError::Wrapped("failed to create bot", err))?;

        self.set_plugin_key(plugin_id, BOT_USER_KEY, Some(created.user_id.as_bytes()))
            .await
            .map_err(|err| EnsureBotError::Wrapped("failed to set plugin key", err))?;

        Ok(created.user_id)
    }

    /// Port of `App.PermanentDeleteBot` (app/bot.go:452): the `Bots` row, then the `Users` row,
    /// and nothing else — no cache purge, no event, no posts touched.
    ///
    /// A bot that does not exist is not an error: both deletes match nothing. The bot half's
    /// failure is 400 `app.bot.permenent_delete.bad_id` (Go's spelling) naming the id, because the
    /// store reports every failure as invalid input; the user half's is a 500.
    ///
    /// **Go's user cache is not invalidated**, so on Go a `GetUser` of the deleted bot keeps
    /// answering the old row until the cache entry expires (measured through the plugin API).
    /// This server has no user cache, so the same read is the 404 the database gives: a stale
    /// read is not reproduced.
    #[tracing::instrument(skip(self))]
    pub async fn permanent_delete_bot(&self, bot_user_id: &str) -> AppResult {
        if let Err(err) = self.store().bot().permanent_delete(bot_user_id).await {
            tracing::warn!(error = %err, "the Bots row could not be deleted");
            let params = std::collections::HashMap::from([(
                "user_id".to_owned(),
                serde_json::Value::String(bot_user_id.to_owned()),
            )]);
            return Err(AppError::boxed(
                "PermanentDeleteBot",
                "app.bot.permenent_delete.bad_id",
                Some(params),
                String::new(),
                400,
            ));
        }
        if let Err(err) = self.store().user().permanent_delete(bot_user_id).await {
            tracing::error!(error = %err, "the bot's Users row could not be deleted");
            return Err(AppError::boxed(
                "PermanentDeleteBot",
                "app.user.permanent_delete.app_error",
                None,
                String::new(),
                500,
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 404 carries the bot's id as a **parameter**, not only in the message. Clients read
    /// `params`, and an error built without it is a different document.
    #[test]
    fn the_not_found_error_is_the_stores_id_with_a_user_id_param() {
        let err = make_bot_not_found_error("SqlBotStore.Get", "rcw3d9njxiy6pquw79ux5wqxjw");
        assert_eq!(err.id, "store.sql_bot.get.missing.app_error");
        assert_eq!(err.status_code, 404);
        assert_eq!(err.where_, "SqlBotStore.Get");
        assert_eq!(
            err.params
                .as_ref()
                .and_then(|p| p.get("user_id"))
                .and_then(|v| v.as_str()),
            Some("rcw3d9njxiy6pquw79ux5wqxjw")
        );
    }

    /// `App.CreateBot` picks between three ids by the **field** the store named, and a fourth for
    /// anything else. Nothing else on this path can tell an email clash from a username clash.
    #[test]
    fn the_create_error_is_chosen_by_the_field_the_store_refused() {
        let for_field = |field: &'static str| {
            create_bot_user_save_error(StoreError::InvalidInput {
                entity: "User",
                field,
                value: "x".to_owned(),
            })
        };
        assert_eq!(
            for_field("email").id,
            "app.user.save.email_exists.app_error"
        );
        assert_eq!(
            for_field("username").id,
            "app.user.save.username_exists.app_error"
        );
        // Neither of the two named fields: Go's `default` arm, and a third id rather than a
        // fallback to one of the others.
        assert_eq!(
            for_field("auth_data").id,
            "app.user.save.existing.app_error"
        );
        for field in ["email", "username", "auth_data"] {
            assert_eq!(for_field(field).status_code, 400, "{field}");
        }

        // A validation failure keeps its own id and status all the way to the client.
        let invalid = create_bot_user_save_error(StoreError::Invalid {
            entity: "User",
            app_error: AppError::boxed(
                "User.IsValid",
                "model.user.is_valid.email.app_error",
                None,
                String::new(),
                400,
            ),
        });
        assert_eq!(invalid.id, "model.user.is_valid.email.app_error");

        let broken = create_bot_user_save_error(StoreError::NotFound {
            entity: "User",
            criteria: "id=x".to_owned(),
        });
        assert_eq!(broken.id, "app.user.save.app_error");
        assert_eq!(broken.status_code, 500);
    }

    /// `PatchBot`'s conflict arm uses the **save** spellings on an update path, and it reads
    /// `Username` — anything else, including `Email`, takes the email branch.
    #[test]
    fn the_patch_conflict_arm_branches_on_the_constraint_name() {
        let for_resource = |resource: &'static str| {
            patch_bot_user_update_error(
                StoreError::Conflict {
                    resource,
                    source: sqlx::Error::RowNotFound,
                },
                "rcw3d9njxiy6pquw79ux5wqxjw",
            )
        };
        assert_eq!(
            for_resource("Username").id,
            "app.user.save.username_exists.app_error"
        );
        assert_eq!(
            for_resource("Email").id,
            "app.user.save.email_exists.app_error"
        );

        // The two non-conflict shapes, which differ from `UpdateUser`'s only in `where`.
        assert_eq!(
            patch_bot_user_update_error(
                StoreError::InvalidInput {
                    entity: "User",
                    field: "id",
                    value: "x".to_owned(),
                },
                "x",
            )
            .id,
            "app.user.update.find.app_error"
        );
        let broken = patch_bot_user_update_error(
            StoreError::NotFound {
                entity: "User",
                criteria: "id=x".to_owned(),
            },
            "x",
        );
        assert_eq!(broken.id, "app.user.update.finding.app_error");
        assert_eq!(broken.status_code, 500);
    }

    /// **A write to a vanished bot is reported as a read miss, byte for byte.** If these diverge,
    /// the write routes become the oracle for which users are bots that the read routes refuse to
    /// be.
    #[test]
    fn a_lost_bot_on_the_write_path_is_the_same_404_as_a_read_miss() {
        let id = "rcw3d9njxiy6pquw79ux5wqxjw";
        let write = bot_update_error(
            "PatchBot",
            "app.bot.patchbot.internal_error",
            StoreError::NotFound {
                entity: "Bot",
                criteria: format!("user_id={id}"),
            },
            id,
        );
        let read = make_bot_not_found_error("SqlBotStore.Get", id);
        assert_eq!(
            serde_json::to_value(&write).expect("serialises"),
            serde_json::to_value(&read).expect("serialises"),
        );
        assert_eq!(write.status_code, 404);

        // Anything else is the caller's own 500, with the id it was given.
        let other = bot_update_error(
            "UpdateBotOwner",
            "app.bot.patchbot.internal_error",
            StoreError::Db {
                context: "boom".to_owned(),
                source: sqlx::Error::RowNotFound,
            },
            id,
        );
        assert_eq!(other.id, "app.bot.patchbot.internal_error");
        assert_eq!(other.status_code, 500);
        assert_eq!(other.where_, "UpdateBotOwner");
    }

    /// `UpdateActive` has **no conflict arm** — three shapes, not five — so a unique-constraint
    /// violation there is a 500 rather than the 400 `PatchBot` gives it.
    #[test]
    fn the_deactivation_error_has_no_conflict_branch() {
        let conflict = update_active_error(StoreError::Conflict {
            resource: "Username",
            source: sqlx::Error::RowNotFound,
        });
        assert_eq!(conflict.id, "app.user.update.finding.app_error");
        assert_eq!(conflict.status_code, 500);
        assert_eq!(
            update_active_error(StoreError::InvalidInput {
                entity: "User",
                field: "id",
                value: "x".to_owned(),
            })
            .status_code,
            400
        );
    }

    /// A list failure is a 500 with a **different id** from the single-bot one — `getbots`, not
    /// `getbot`. One character, and it is what a client branches on.
    #[test]
    fn the_list_error_is_its_own_id() {
        let err = get_bots_error(StoreError::NotFound {
            entity: "Bot",
            criteria: "user_id=x".to_owned(),
        });
        assert_eq!(err.id, "app.bot.getbots.internal_error");
        assert_eq!(err.status_code, 500);
    }

    fn unreachable_app() -> crate::App {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nothing")
            .expect("a lazy pool is built without connecting");
        crate::App::new(mm_store::SqlStore::from_pool(pool))
    }

    /// `EnsureBot`'s two refusals come before the KV read, so neither touches the store; the KV
    /// read's own failure is returned as the app error it is, not wrapped.
    #[tokio::test]
    async fn ensure_bot_refuses_before_it_reads() {
        let app = unreachable_app();
        let nil = app.ensure_bot("p", None).await.expect_err("nil");
        assert_eq!(nil.to_string(), "passed a nil bot");
        assert!(matches!(nil, EnsureBotError::Message(_)));

        let nameless = Bot {
            username: String::new(),
            ..Bot::default()
        };
        let err = app
            .ensure_bot("p", Some(&nameless))
            .await
            .expect_err("no name");
        assert_eq!(err.to_string(), "passed a bot with no username");

        let named = Bot {
            username: "named".into(),
            ..Bot::default()
        };
        match app.ensure_bot("p", Some(&named)).await {
            Err(EnsureBotError::App(err)) => {
                assert_eq!(err.id, "app.plugin_store.get.app_error");
            }
            other => panic!("the KV read's failure, unwrapped: {other:?}"),
        }
    }

    /// `fmt.Errorf("%s: %w")` over an app error renders the app error's `Error()`.
    #[test]
    fn a_wrapped_ensure_bot_error_renders_as_go_formats_it() {
        let err = EnsureBotError::Wrapped(
            "failed to create bot",
            AppError::boxed("CreateBot", "some.id", None, "detail".to_owned(), 400),
        );
        assert_eq!(
            err.to_string(),
            "failed to create bot: CreateBot: some.id, detail"
        );
        assert_eq!(BOT_USER_KEY, "mmi_botid");
    }

    /// The bot half's failure is the 400 with Go's misspelt id, naming the bot.
    #[tokio::test]
    async fn a_failed_bot_delete_is_the_bad_id_400() {
        let err = unreachable_app()
            .permanent_delete_bot("abcdefghijklmnopqrstuvwxyz")
            .await
            .expect_err("the store is unreachable");
        assert_eq!(err.id, "app.bot.permenent_delete.bad_id");
        assert_eq!(err.status_code, 400);
        assert_eq!(err.where_, "PermanentDeleteBot");
        assert_eq!(
            err.params.as_ref().and_then(|p| p.get("user_id")),
            Some(&serde_json::json!("abcdefghijklmnopqrstuvwxyz"))
        );
    }
}

#[cfg(test)]
mod dm_exemption_tests {
    use super::*;

    fn bot(username: &str, owner: &str) -> Bot {
        Bot {
            user_id: "botid".to_owned(),
            username: username.to_owned(),
            owner_id: owner.to_owned(),
            ..Bot::default()
        }
    }

    fn never() -> AppResult<Vec<String>> {
        Err(AppError::boxed(
            "t",
            "the plugin list was consulted",
            None,
            "",
            500,
        ))
    }

    /// The system bot is exempt first — before the owner, before the plugin host is asked.
    #[test]
    fn the_system_bot_is_exempt_whoever_asks() {
        let system = bot(BOT_SYSTEM_BOT_USERNAME, "plugin.x");
        for hosted in [true, false] {
            assert_eq!(
                bot_exemption(&system, None, hosted, true, never).expect("decided"),
                BotExemption::Exempt
            );
        }
    }

    /// A bot the **session's** user owns is exempt; no session owns nothing.
    #[test]
    fn the_sessions_own_bot_is_exempt() {
        let owned = bot("helper", "owner1");
        assert_eq!(
            bot_exemption(&owned, Some("owner1"), true, true, never).expect("decided"),
            BotExemption::Exempt
        );
        assert_eq!(
            bot_exemption(&owned, Some("someone"), false, false, never).expect("decided"),
            BotExemption::NotExempt
        );
        assert_eq!(
            bot_exemption(&owned, None, false, false, never).expect("decided"),
            BotExemption::NotExempt
        );
    }

    /// Under the Go host the plugin half is Go's: decided only when plugins are off.
    #[test]
    fn under_the_go_host_only_plugins_off_is_decided() {
        let plugin_bot = bot("pbot", "com.example.p");
        assert_eq!(
            bot_exemption(&plugin_bot, Some("u"), false, false, never).expect("decided"),
            BotExemption::NotExempt
        );
        assert_eq!(
            bot_exemption(&plugin_bot, Some("u"), false, true, never).expect("decided"),
            BotExemption::Undecidable
        );
    }

    /// Hosted here: the owner must be the id of an available plugin, and a listing failure is
    /// the error.
    #[test]
    fn hosted_here_the_owner_must_be_an_available_plugin() {
        let plugin_bot = bot("pbot", "com.example.p");
        let listed = || Ok(vec!["com.other".to_owned(), "com.example.p".to_owned()]);
        assert_eq!(
            bot_exemption(&plugin_bot, Some("u"), true, true, listed).expect("decided"),
            BotExemption::Exempt
        );
        let others = || Ok(vec!["com.other".to_owned()]);
        assert_eq!(
            bot_exemption(&plugin_bot, Some("u"), true, true, others).expect("decided"),
            BotExemption::NotExempt
        );
        let err = bot_exemption(&plugin_bot, Some("u"), true, true, never).expect_err("listing");
        assert_eq!(err.id, "the plugin list was consulted");
    }
}
