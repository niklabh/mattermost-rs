//! Port of the `App` functions behind `getIncomingHooks` and `getOutgoingHooks`
//! (channels/app/webhook.go:645, :658, :675, :820, :833, :850).
//!
//! Each is the same shape: a config gate answering **501**, then one store call wrapped into a
//! 500. The gate is checked *per call*, which matters for `include_total_count` — the handler
//! makes two of them and either can be the one that refuses.
//!
//! # The ids are not symmetric, and one of them is a Go copy-paste
//!
//! The incoming trio share `app.webhooks.get_incoming_by_user.app_error` (and the count has its
//! own). The outgoing trio do **not**: the team function reports
//! `app.webhooks.get_outgoing_by_team.app_error` while *both* the channel function and the
//! **whole-server list** report `app.webhooks.get_outgoing_by_channel.app_error`
//! (webhook.go:827) — the unscoped list naming a channel it never had. Reproduced verbatim.
//! Tidying it would make our error id differ from the server we forward to.

use mm_model::incoming_webhook::IncomingWebhook;
use mm_model::outgoing_webhook::OutgoingWebhook;
use mm_model::utils::{AppError, AppResult};
use mm_store::{StoreError, WebhookStore};

use crate::App;

/// `api.incoming_webhook.disabled.app_error` — a **501**, not a 403.
///
/// The distinction is on the wire and clients branch on it: a disabled feature is "this server
/// does not do that", where a permission failure is "not you". Go uses `StatusNotImplemented`
/// for every one of the three.
const DISABLED_ERROR: &str = "api.incoming_webhook.disabled.app_error";

impl App {
    /// Port of `App.GetIncomingWebhooksForTeamPageByUser` (webhook.go:645).
    #[tracing::instrument(skip_all, fields(team_id, user_id, page, per_page, found))]
    pub async fn get_incoming_webhooks_for_team_page_by_user(
        &self,
        team_id: &str,
        user_id: &str,
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<IncomingWebhook>> {
        if !self.config().enable_incoming_webhooks {
            return Err(disabled("GetIncomingWebhooksForTeamPage"));
        }

        let hooks = self
            .store()
            .webhook()
            .get_incoming_by_team_by_user(team_id, user_id, page * per_page, per_page)
            .await
            .map_err(|err| store_failure("GetIncomingWebhooksForTeamPage", err))?;

        tracing::Span::current().record("found", hooks.len());
        Ok(hooks)
    }

    /// Port of `App.GetIncomingWebhooksPageByUser` (webhook.go:658).
    ///
    /// Note the `where` string Go reports differs between the two — `GetIncomingWebhooksForTeamPage`
    /// (without the `ByUser` suffix its function name carries) versus
    /// `GetIncomingWebhooksPageByUser`. Both are reproduced verbatim; the field is not on the wire
    /// today, but it is the one part of an `AppError` that says which of two near-identical
    /// functions produced it.
    #[tracing::instrument(skip_all, fields(user_id, page, per_page, found))]
    pub async fn get_incoming_webhooks_page_by_user(
        &self,
        user_id: &str,
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<IncomingWebhook>> {
        if !self.config().enable_incoming_webhooks {
            return Err(disabled("GetIncomingWebhooksPageByUser"));
        }

        let hooks = self
            .store()
            .webhook()
            .get_incoming_list_by_user(user_id, page * per_page, per_page)
            .await
            .map_err(|err| store_failure("GetIncomingWebhooksPageByUser", err))?;

        tracing::Span::current().record("found", hooks.len());
        Ok(hooks)
    }

    /// Port of `App.GetIncomingWebhooksCount` (webhook.go:675).
    ///
    /// Its 500 carries a **different id** from the two list functions —
    /// `app.webhooks.get_incoming_count.app_error` — and a params map naming `TeamID`, `UserID`
    /// and the underlying error. The params are dropped here: Go interpolates them into a
    /// translated message we do not produce ([D-092]), and carrying the raw store error into a
    /// client-visible map would leak query detail that `WipeDetailed` exists to withhold.
    #[tracing::instrument(skip_all, fields(team_id, user_id, count))]
    pub async fn get_incoming_webhooks_count(
        &self,
        team_id: &str,
        user_id: &str,
    ) -> AppResult<i64> {
        if !self.config().enable_incoming_webhooks {
            return Err(disabled("GetIncomingWebhooksCount"));
        }

        let count = self
            .store()
            .webhook()
            .analytics_incoming_count(team_id, user_id)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "counting incoming webhooks failed");
                AppError::boxed(
                    "GetIncomingWebhooksCount",
                    "app.webhooks.get_incoming_count.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        tracing::Span::current().record("count", count);
        Ok(count)
    }
}

fn disabled(where_: &str) -> Box<AppError> {
    AppError::boxed(where_, DISABLED_ERROR, None, String::new(), 501)
}

/// The two list functions share one id, `app.webhooks.get_incoming_by_user.app_error`, and differ
/// only in `where`.
fn store_failure(where_: &str, err: StoreError) -> Box<AppError> {
    tracing::error!(caller = where_, error = ?err, "incoming webhook lookup failed");
    AppError::boxed(
        where_,
        "app.webhooks.get_incoming_by_user.app_error",
        None,
        String::new(),
        500,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A disabled feature is **501**, and it is the same id from all three entry points — but
    /// three different `where`s, which is the only thing separating them in a log.
    #[test]
    fn the_disabled_gate_is_a_501_with_one_id_and_three_wheres() {
        for where_ in [
            "GetIncomingWebhooksForTeamPage",
            "GetIncomingWebhooksPageByUser",
            "GetIncomingWebhooksCount",
        ] {
            let err = disabled(where_);
            assert_eq!(err.status_code, 501, "{where_}: not a 403");
            assert_eq!(err.id, DISABLED_ERROR, "{where_}");
            assert_eq!(err.where_, where_);
        }
    }

    /// The count's failure id differs from the lists' — a port that reused one would answer the
    /// wrong `id` for half the route.
    #[test]
    fn the_count_and_the_list_do_not_share_a_failure_id() {
        let list = store_failure(
            "GetIncomingWebhooksPageByUser",
            StoreError::Db {
                context: "boom".to_owned(),
                source: sqlx::Error::RowNotFound,
            },
        );
        assert_eq!(list.id, "app.webhooks.get_incoming_by_user.app_error");
        assert_eq!(list.status_code, 500);
        assert_ne!(list.id, "app.webhooks.get_incoming_count.app_error");
    }
}

impl App {
    /// Port of `App.GetOutgoingWebhooksPageByUser` (webhook.go:820).
    #[tracing::instrument(skip_all, fields(user_id, page, per_page, found))]
    pub async fn get_outgoing_webhooks_page_by_user(
        &self,
        user_id: &str,
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<OutgoingWebhook>> {
        if !self.config().enable_outgoing_webhooks {
            return Err(outgoing_disabled("GetOutgoingWebhooksPageByUser"));
        }

        let hooks = self
            .store()
            .webhook()
            .get_outgoing_list_by_user(user_id, page * per_page, per_page)
            .await
            .map_err(|err| {
                outgoing_store_failure(
                    "GetOutgoingWebhooksPageByUser",
                    OUTGOING_BY_CHANNEL_ERROR,
                    err,
                )
            })?;

        tracing::Span::current().record("found", hooks.len());
        Ok(hooks)
    }

    /// Port of `App.GetOutgoingWebhooksForChannelPageByUser` (webhook.go:833).
    ///
    /// Its `where` is `GetOutgoingWebhooksForChannelPage` — without the `ByUser` its own name
    /// carries, exactly as the incoming team function drops it.
    #[tracing::instrument(skip_all, fields(channel_id, user_id, page, per_page, found))]
    pub async fn get_outgoing_webhooks_for_channel_page_by_user(
        &self,
        channel_id: &str,
        user_id: &str,
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<OutgoingWebhook>> {
        if !self.config().enable_outgoing_webhooks {
            return Err(outgoing_disabled("GetOutgoingWebhooksForChannelPage"));
        }

        let hooks = self
            .store()
            .webhook()
            .get_outgoing_by_channel_by_user(channel_id, user_id, page * per_page, per_page)
            .await
            .map_err(|err| {
                outgoing_store_failure(
                    "GetOutgoingWebhooksForChannelPage",
                    OUTGOING_BY_CHANNEL_ERROR,
                    err,
                )
            })?;

        tracing::Span::current().record("found", hooks.len());
        Ok(hooks)
    }

    /// Port of `App.GetOutgoingWebhooksForTeamPageByUser` (webhook.go:850) — the one outgoing
    /// function with an id of its own.
    #[tracing::instrument(skip_all, fields(team_id, user_id, page, per_page, found))]
    pub async fn get_outgoing_webhooks_for_team_page_by_user(
        &self,
        team_id: &str,
        user_id: &str,
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<OutgoingWebhook>> {
        if !self.config().enable_outgoing_webhooks {
            return Err(outgoing_disabled("GetOutgoingWebhooksForTeamPageByUser"));
        }

        let hooks = self
            .store()
            .webhook()
            .get_outgoing_by_team_by_user(team_id, user_id, page * per_page, per_page)
            .await
            .map_err(|err| {
                outgoing_store_failure(
                    "GetOutgoingWebhooksForTeamPageByUser",
                    OUTGOING_BY_TEAM_ERROR,
                    err,
                )
            })?;

        tracing::Span::current().record("found", hooks.len());
        Ok(hooks)
    }
}

/// `api.outgoing_webhook.disabled.app_error` — a **different id** from the incoming one, and the
/// same 501.
const OUTGOING_DISABLED_ERROR: &str = "api.outgoing_webhook.disabled.app_error";
/// Reported by the channel function **and** by the unscoped list (webhook.go:827) — see the
/// module note.
const OUTGOING_BY_CHANNEL_ERROR: &str = "app.webhooks.get_outgoing_by_channel.app_error";
const OUTGOING_BY_TEAM_ERROR: &str = "app.webhooks.get_outgoing_by_team.app_error";

fn outgoing_disabled(where_: &str) -> Box<AppError> {
    AppError::boxed(where_, OUTGOING_DISABLED_ERROR, None, String::new(), 501)
}

fn outgoing_store_failure(where_: &str, id: &'static str, err: StoreError) -> Box<AppError> {
    tracing::error!(caller = where_, error = ?err, "outgoing webhook lookup failed");
    AppError::boxed(where_, id, None, String::new(), 500)
}

#[cfg(test)]
mod outgoing_tests {
    use super::*;

    /// The incoming and outgoing disabled errors are **different ids** at the same status. A port
    /// that shared one would tell an integrations page the wrong feature was off.
    #[test]
    fn the_two_disabled_ids_are_distinct() {
        assert_ne!(DISABLED_ERROR, OUTGOING_DISABLED_ERROR);
        assert_eq!(outgoing_disabled("x").status_code, 501);
        assert_eq!(outgoing_disabled("x").id, OUTGOING_DISABLED_ERROR);
    }

    /// **The unscoped outgoing list reports a *channel* error id.** It is Go's copy-paste
    /// (webhook.go:827) and it is deliberate here; this test exists so that "fixing" it fails.
    #[test]
    fn the_unscoped_outgoing_list_reports_the_channel_id() {
        assert_eq!(
            OUTGOING_BY_CHANNEL_ERROR,
            "app.webhooks.get_outgoing_by_channel.app_error"
        );
        assert_ne!(OUTGOING_BY_CHANNEL_ERROR, OUTGOING_BY_TEAM_ERROR);
    }
}

impl App {
    /// Port of `App.GetIncomingWebhook` (webhook.go:622).
    ///
    /// # One id, two statuses
    ///
    /// `app.webhooks.get_incoming.app_error` is **both** the 404 and the 500 — Go's `switch`
    /// changes the status and leaves the id alone (webhook.go:632, :634). A client cannot tell
    /// "no such hook" from "the database is down" by the id, only by the status line, and a port
    /// that gave the two different ids would be inventing a distinction Go does not make.
    #[tracing::instrument(skip_all, fields(hook_id))]
    pub async fn get_incoming_webhook(&self, hook_id: &str) -> AppResult<IncomingWebhook> {
        if !self.config().enable_incoming_webhooks {
            return Err(disabled("GetIncomingWebhook"));
        }

        self.store()
            .webhook()
            .get_incoming(hook_id)
            .await
            .map_err(|err| single_hook_error("GetIncomingWebhook", INCOMING_GET_ERROR, err))
    }

    /// Port of `App.GetOutgoingWebhook` (webhook.go:797) — the same shape, its own id.
    #[tracing::instrument(skip_all, fields(hook_id))]
    pub async fn get_outgoing_webhook(&self, hook_id: &str) -> AppResult<OutgoingWebhook> {
        if !self.config().enable_outgoing_webhooks {
            return Err(outgoing_disabled("GetOutgoingWebhook"));
        }

        self.store()
            .webhook()
            .get_outgoing(hook_id)
            .await
            .map_err(|err| single_hook_error("GetOutgoingWebhook", OUTGOING_GET_ERROR, err))
    }
}

const INCOMING_GET_ERROR: &str = "app.webhooks.get_incoming.app_error";
const OUTGOING_GET_ERROR: &str = "app.webhooks.get_outgoing.app_error";

/// Go's `errors.As(err, &nfErr)` split: **404 for not-found, 500 for anything else, same id**.
fn single_hook_error(where_: &str, id: &'static str, err: StoreError) -> Box<AppError> {
    let not_found = err.is_not_found();
    if !not_found {
        tracing::error!(caller = where_, error = ?err, "single webhook lookup failed");
    }
    AppError::boxed(
        where_,
        id,
        None,
        String::new(),
        if not_found { 404 } else { 500 },
    )
}

#[cfg(test)]
mod single_hook_tests {
    use super::*;

    /// The id is the same on both arms and only the status moves. Asserted because the obvious
    /// "improvement" — a distinct `not_found` id — is a wire change.
    #[test]
    fn not_found_and_failure_share_an_id_and_differ_only_in_status() {
        let missing = single_hook_error(
            "GetIncomingWebhook",
            INCOMING_GET_ERROR,
            StoreError::NotFound {
                entity: "IncomingWebhook",
                criteria: "id=x".to_owned(),
            },
        );
        let broken = single_hook_error(
            "GetIncomingWebhook",
            INCOMING_GET_ERROR,
            StoreError::Db {
                context: "boom".to_owned(),
                source: sqlx::Error::RowNotFound,
            },
        );

        assert_eq!(missing.id, broken.id, "one id, two statuses");
        assert_eq!(missing.status_code, 404);
        assert_eq!(broken.status_code, 500);
    }

    /// And the two hook kinds do not share theirs.
    #[test]
    fn the_two_single_hook_ids_are_distinct() {
        assert_ne!(INCOMING_GET_ERROR, OUTGOING_GET_ERROR);
    }
}

impl App {
    /// Port of `app.App.CreateIncomingWebhookForChannel` (app/webhook.go).
    ///
    /// # The two override settings are applied differently here and in the update path
    ///
    /// With `EnablePostUsernameOverride` off, **create blanks the username to `""`** while
    /// [`App::update_incoming_webhook`] restores the *old hook's* value. Same setting, opposite
    /// effect on an existing configuration: turning the setting off does not erase usernames
    /// already stored, it makes them unchangeable. Both defaults are `false`, so the blanking is
    /// what a stock server does.
    ///
    /// The username is validated **after** the blanking, so an invalid username on a server with
    /// the override off is silently dropped rather than refused.
    ///
    /// `UserId` and `TeamId` are overwritten from the caller and the channel — a client cannot
    /// choose either.
    #[tracing::instrument(skip(self, hook), fields(channel_id = %channel.id))]
    pub async fn create_incoming_webhook_for_channel(
        &self,
        creator_id: &str,
        channel: &mm_model::channel::Channel,
        hook: &IncomingWebhook,
    ) -> AppResult<IncomingWebhook> {
        if !self.config().enable_incoming_webhooks {
            return Err(incoming_disabled("CreateIncomingWebhookForChannel"));
        }

        let mut hook = hook.clone();
        hook.user_id = creator_id.to_owned();
        hook.team_id = channel.team_id.clone();

        if !self.config().enable_post_username_override {
            hook.username = String::new();
        }
        if !self.config().enable_post_icon_override {
            hook.icon_url = String::new();
        }

        if !hook.username.is_empty() && !mm_model::user::is_valid_username(&hook.username) {
            return Err(AppError::boxed(
                "CreateIncomingWebhookForChannel",
                "api.incoming_webhook.invalid_username.app_error",
                None,
                String::new(),
                400,
            ));
        }

        // `SaveIncoming` refuses a hook that already carries an id, before `PreSave` runs — so a
        // client that echoes back an existing hook gets a 400 rather than overwriting it.
        if !hook.id.is_empty() {
            return Err(AppError::boxed(
                "CreateIncomingWebhookForChannel",
                "app.webhooks.save_incoming.existing.app_error",
                None,
                String::new(),
                400,
            ));
        }

        hook.pre_save();
        hook.is_valid()?;

        self.store()
            .webhook()
            .save_incoming(&hook)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "incoming webhook save failed");
                AppError::boxed(
                    "CreateIncomingWebhookForChannel",
                    "app.webhooks.save_incoming.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        Ok(hook)
    }

    /// Port of `app.App.UpdateIncomingWebhook` (app/webhook.go).
    ///
    /// Seven fields are copied off the old hook and cannot be changed: `Id`, `UserId`,
    /// `CreateAt`, `TeamId`, `DeleteAt` and `LastUsed`, with `UpdateAt` taken fresh. The store's
    /// `SET` list omits `UserId` and `LastUsed` as well, so both are protected twice.
    #[tracing::instrument(skip(self, old_hook, updated_hook), fields(id = %old_hook.id))]
    pub async fn update_incoming_webhook(
        &self,
        old_hook: &IncomingWebhook,
        updated_hook: &IncomingWebhook,
    ) -> AppResult<IncomingWebhook> {
        if !self.config().enable_incoming_webhooks {
            return Err(incoming_disabled("UpdateIncomingWebhook"));
        }

        let mut hook = updated_hook.clone();

        // **Restores the old value rather than blanking**, unlike the create path above.
        if !self.config().enable_post_username_override {
            hook.username = old_hook.username.clone();
        }
        if !self.config().enable_post_icon_override {
            hook.icon_url = old_hook.icon_url.clone();
        }

        if !hook.username.is_empty() && !mm_model::user::is_valid_username(&hook.username) {
            return Err(AppError::boxed(
                "UpdateIncomingWebhook",
                "api.incoming_webhook.invalid_username.app_error",
                None,
                String::new(),
                400,
            ));
        }

        hook.id = old_hook.id.clone();
        hook.user_id = old_hook.user_id.clone();
        hook.create_at = old_hook.create_at;
        hook.update_at = mm_model::utils::get_millis();
        hook.team_id = old_hook.team_id.clone();
        hook.delete_at = old_hook.delete_at;
        hook.last_used = old_hook.last_used;

        self.store()
            .webhook()
            .update_incoming(&hook)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "incoming webhook update failed");
                AppError::boxed(
                    "UpdateIncomingWebhook",
                    "app.webhooks.update_incoming.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        // `InvalidateCacheForWebhook` clears Go's own webhook cache. There is nothing to clear
        // here, and Go's copy is unreachable from this process — see [D-190].
        Ok(hook)
    }

    /// Port of `app.App.DeleteIncomingWebhook` (app/webhook.go).
    #[tracing::instrument(skip(self), fields(id = %hook_id))]
    pub async fn delete_incoming_webhook(&self, hook_id: &str) -> AppResult<()> {
        if !self.config().enable_incoming_webhooks {
            return Err(incoming_disabled("DeleteIncomingWebhook"));
        }

        self.store()
            .webhook()
            .delete_incoming(hook_id, mm_model::utils::get_millis())
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "incoming webhook delete failed");
                AppError::boxed(
                    "DeleteIncomingWebhook",
                    "app.webhooks.delete_incoming.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `app.App.CreateOutgoingWebhook` (app/webhook.go).
    ///
    /// # A channel-scoped hook must be in an open channel, and Go says so twice
    ///
    /// ```text
    /// if channel.Type != Open                          -> 403 api.outgoing_webhook.disabled
    /// if channel.Type != Open || channel.TeamId != ...  -> 403 api.webhook.create_outgoing.permissions
    /// ```
    ///
    /// The second condition's first disjunct is **unreachable** — the first `if` already returned
    /// — so the team mismatch is the only way to reach the second error. Reproduced as written,
    /// because the two ids differ and a client branches on them.
    ///
    /// A hook with **no** channel must carry trigger words instead; that arm is a **400** where
    /// the update path's identical arm is a 500.
    #[tracing::instrument(skip(self, hook), fields(team_id = %hook.team_id))]
    pub async fn create_outgoing_webhook(
        &self,
        hook: &OutgoingWebhook,
    ) -> AppResult<OutgoingWebhook> {
        if !self.config().enable_outgoing_webhooks {
            return Err(outgoing_disabled("CreateOutgoingWebhook"));
        }

        let mut hook = hook.clone();

        if !hook.channel_id.is_empty() {
            let channel = self.get_channel(&hook.channel_id).await.map_err(|err| {
                let mut params: std::collections::HashMap<String, serde_json::Value> =
                    std::collections::HashMap::new();
                params.insert(
                    "channel_id".to_owned(),
                    serde_json::Value::String(hook.channel_id.clone()),
                );
                let (id, status) = if err.status_code == 404 {
                    ("app.channel.get.existing.app_error", 404)
                } else {
                    ("app.channel.get.find.app_error", 500)
                };
                AppError::boxed(
                    "CreateOutgoingWebhook",
                    id,
                    Some(params),
                    String::new(),
                    status,
                )
            })?;

            if channel.channel_type != mm_model::channel::CHANNEL_TYPE_OPEN {
                return Err(outgoing_disabled_forbidden("CreateOutgoingWebhook"));
            }

            if channel.team_id != hook.team_id {
                return Err(AppError::boxed(
                    "CreateOutgoingWebhook",
                    "api.webhook.create_outgoing.permissions.app_error",
                    None,
                    String::new(),
                    403,
                ));
            }
        } else if hook
            .trigger_words
            .as_ref()
            .is_none_or(|words| words.is_empty())
        {
            return Err(AppError::boxed(
                "CreateOutgoingWebhook",
                "api.webhook.create_outgoing.triggers.app_error",
                None,
                String::new(),
                400,
            ));
        }

        self.refuse_intersecting_outgoing(&hook, None, "CreateOutgoingWebhook", 500)
            .await?;

        if !hook.id.is_empty() {
            return Err(AppError::boxed(
                "CreateOutgoingWebhook",
                "app.webhooks.save_outgoing.override.app_error",
                None,
                String::new(),
                400,
            ));
        }

        hook.pre_save();
        hook.is_valid()?;

        self.store()
            .webhook()
            .save_outgoing(&hook)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "outgoing webhook save failed");
                AppError::boxed(
                    "CreateOutgoingWebhook",
                    "app.webhooks.save_outgoing.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        Ok(hook)
    }

    /// Port of `app.App.UpdateOutgoingWebhook` (app/webhook.go).
    ///
    /// The same four gates as the create path with **three differences**, each of which a reader
    /// would smooth over:
    ///
    /// - a non-open channel is `api.webhook.create_outgoing.not_open.app_error`, a *different id*
    ///   from create's;
    /// - the team check compares against the **old** hook's team, not the submitted one;
    /// - the empty-trigger-words arm is a **500**, where create's is a 400. Same message, same
    ///   condition, different status.
    ///
    /// The intersection check additionally excludes the hook being updated, or every update would
    /// collide with itself.
    #[tracing::instrument(skip(self, old_hook, updated_hook), fields(id = %old_hook.id))]
    pub async fn update_outgoing_webhook(
        &self,
        old_hook: &OutgoingWebhook,
        updated_hook: &OutgoingWebhook,
    ) -> AppResult<OutgoingWebhook> {
        if !self.config().enable_outgoing_webhooks {
            return Err(outgoing_disabled("UpdateOutgoingWebhook"));
        }

        let mut hook = updated_hook.clone();

        if !hook.channel_id.is_empty() {
            let channel = self.get_channel(&hook.channel_id).await?;

            if channel.channel_type != mm_model::channel::CHANNEL_TYPE_OPEN {
                return Err(AppError::boxed(
                    "UpdateOutgoingWebhook",
                    "api.webhook.create_outgoing.not_open.app_error",
                    None,
                    String::new(),
                    403,
                ));
            }

            if channel.team_id != old_hook.team_id {
                return Err(AppError::boxed(
                    "UpdateOutgoingWebhook",
                    "api.webhook.create_outgoing.permissions.app_error",
                    None,
                    String::new(),
                    403,
                ));
            }
        } else if hook
            .trigger_words
            .as_ref()
            .is_none_or(|words| words.is_empty())
        {
            return Err(AppError::boxed(
                "UpdateOutgoingWebhook",
                "api.webhook.create_outgoing.triggers.app_error",
                None,
                String::new(),
                500,
            ));
        }

        self.refuse_intersecting_outgoing(
            &hook,
            Some(&old_hook.team_id),
            "UpdateOutgoingWebhook",
            400,
        )
        .await?;

        hook.creator_id = old_hook.creator_id.clone();
        hook.create_at = old_hook.create_at;
        hook.delete_at = old_hook.delete_at;
        hook.team_id = old_hook.team_id.clone();
        hook.update_at = mm_model::utils::get_millis();

        self.store()
            .webhook()
            .update_outgoing(&hook)
            .await
            .map_err(|err| update_outgoing_error("UpdateOutgoingWebhook", err))?;

        Ok(hook)
    }

    /// Port of `app.App.DeleteOutgoingWebhook` (app/webhook.go).
    #[tracing::instrument(skip(self), fields(id = %hook_id))]
    pub async fn delete_outgoing_webhook(&self, hook_id: &str) -> AppResult<()> {
        if !self.config().enable_outgoing_webhooks {
            return Err(outgoing_disabled("DeleteOutgoingWebhook"));
        }

        self.store()
            .webhook()
            .delete_outgoing(hook_id, mm_model::utils::get_millis())
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "outgoing webhook delete failed");
                AppError::boxed(
                    "DeleteOutgoingWebhook",
                    "app.webhooks.delete_outgoing.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `app.App.RegenOutgoingWebhookToken` (app/webhook.go).
    ///
    /// A new token and **nothing else** — no `UpdateAt`, no validation, straight through
    /// `UpdateOutgoing`, whose `SET` list happens to cover every column so the unchanged ones are
    /// rewritten with their own values.
    #[tracing::instrument(skip(self, hook), fields(id = %hook.id))]
    pub async fn regen_outgoing_webhook_token(
        &self,
        hook: &OutgoingWebhook,
    ) -> AppResult<OutgoingWebhook> {
        if !self.config().enable_outgoing_webhooks {
            return Err(outgoing_disabled("RegenOutgoingWebhookToken"));
        }

        let mut hook = hook.clone();
        hook.token = mm_model::utils::new_id();

        self.store()
            .webhook()
            .update_outgoing(&hook)
            .await
            .map_err(|err| update_outgoing_error("RegenOutgoingWebhookToken", err))?;

        Ok(hook)
    }

    /// The trigger-word/callback intersection check both outgoing write paths run
    /// (app/webhook.go).
    ///
    /// A new hook collides with an existing one when they share a channel **and** at least one
    /// callback URL **and** at least one trigger word. All three, so two hooks on the same channel
    /// with different triggers are fine.
    ///
    /// `exclude_team` carries the *old* hook's team on the update path, because that is the team
    /// Go scans — not the submitted one. `status` is 500 on create and 400 on update, for the
    /// same condition.
    async fn refuse_intersecting_outgoing(
        &self,
        hook: &OutgoingWebhook,
        old_team_id: Option<&str>,
        where_: &'static str,
        status: i32,
    ) -> AppResult<()> {
        let team_id = old_team_id.unwrap_or(&hook.team_id);
        let existing = self
            .store()
            .webhook()
            .get_outgoing_by_team_unpaged(team_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "outgoing webhook scan failed");
                AppError::boxed(
                    where_,
                    "app.webhooks.get_outgoing_by_team.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        let id = if old_team_id.is_some() {
            // The update path excludes the hook being updated; the create path has no id yet.
            Some(hook.id.as_str())
        } else {
            None
        };

        for other in &existing {
            if other.channel_id != hook.channel_id {
                continue;
            }
            if id == Some(other.id.as_str()) {
                continue;
            }
            if intersects(other.callback_urls.as_ref(), hook.callback_urls.as_ref())
                && intersects(other.trigger_words.as_ref(), hook.trigger_words.as_ref())
            {
                let id = if old_team_id.is_some() {
                    "api.webhook.update_outgoing.intersect.app_error"
                } else {
                    "api.webhook.create_outgoing.intersect.app_error"
                };
                return Err(AppError::boxed(where_, id, None, String::new(), status));
            }
        }

        Ok(())
    }
}

/// `utils.StringArrayIntersection` reduced to the question both call sites ask: is it non-empty.
fn intersects(
    a: Option<&mm_model::utils::StringArray>,
    b: Option<&mm_model::utils::StringArray>,
) -> bool {
    let (Some(a), Some(b)) = (a, b) else {
        return false;
    };
    a.iter().any(|item| b.contains(item))
}

fn incoming_disabled(where_: &'static str) -> Box<AppError> {
    AppError::boxed(
        where_,
        "api.incoming_webhook.disabled.app_error",
        None,
        String::new(),
        501,
    )
}

/// The **same id at a different status**: `CreateOutgoingWebhook` reuses
/// `api.outgoing_webhook.disabled.app_error` for a non-open channel, at **403** rather than the
/// 501 the feature gate uses. One id, two meanings, in one function.
fn outgoing_disabled_forbidden(where_: &'static str) -> Box<AppError> {
    AppError::boxed(
        where_,
        "api.outgoing_webhook.disabled.app_error",
        None,
        String::new(),
        403,
    )
}

fn update_outgoing_error(where_: &'static str, err: StoreError) -> Box<AppError> {
    tracing::error!(error = %err, "outgoing webhook update failed");
    AppError::boxed(
        where_,
        "app.webhooks.update_outgoing.app_error",
        None,
        String::new(),
        500,
    )
}

impl App {
    /// Port of `app.App.ValidateIncomingWebhookUser` (app/webhook.go:521) and the channel-access
    /// half it delegates to.
    ///
    /// Two refusals, both **403** and both carrying the ids in `detailed_error`:
    ///
    /// - assigning a **system admin** as a hook's owner requires the requester to hold
    ///   `manage_system` themselves, so an integration manager cannot forge posts as an admin;
    /// - the owner must be able to read the channel — checked as *that user*, not the requester.
    #[tracing::instrument(skip(self, session, user, channel), fields(user_id = %user.id, channel_id = %channel.id))]
    pub async fn validate_incoming_webhook_user(
        &self,
        session: &mm_model::session::Session,
        user: &mm_model::user::User,
        channel: &mm_model::channel::Channel,
    ) -> AppResult<()> {
        if user.is_system_admin()
            && !self
                .session_has_permission_to(session, &mm_model::permission::PERMISSION_MANAGE_SYSTEM)
                .await
        {
            return Err(AppError::boxed(
                "ValidateIncomingWebhookUser",
                "api.webhook.incoming.user_role.app_error",
                None,
                format!("user_id={}", user.id),
                403,
            ));
        }

        let (has_permission, _) = self
            .has_permission_to_channel(
                &user.id,
                &channel.id,
                &mm_model::permission::PERMISSION_READ_CHANNEL_CONTENT,
            )
            .await;
        if !has_permission {
            return Err(AppError::boxed(
                "ValidateIncomingWebhookUserChannelAccess",
                "api.webhook.incoming.user_membership.app_error",
                None,
                format!("user_id={}, channel_id={}", user.id, channel.id),
                403,
            ));
        }

        Ok(())
    }
}
