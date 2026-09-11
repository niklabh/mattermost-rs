//! Port of the write side of `server/channels/app/channel.go` for the five channel-lifecycle
//! routes: `UpdateChannel`, `PatchChannel`, `UpdateChannelPrivacy`, `DeleteChannel` and
//! `RestoreChannel`.
//!
//! # Every one of them ends in a `Publish`, and three of them address it differently
//!
//! `channel_updated` is addressed to the **channel**; `channel_deleted` and `channel_restored`
//! are addressed to the **team** for a public channel and to the channel for a private one;
//! `channel_converted` is addressed to the team unconditionally. So a private channel's archive
//! reaches only its members while a public channel's reaches everyone on the team — and getting
//! that backwards leaks a private channel's existence to the whole team, which no test of the
//! HTTP body would notice.
//!
//! None of the five passes an `omit_connection_id`: Go hands `NewWebSocketEvent` an empty string
//! for it on every one of these paths, so unlike `upsertDraft` the client's `Connection-Id`
//! header does **not** stop its own tab being told. Asserted in the parity suite.
//!
//! # Six system posts, and one of them can undo the write it follows
//!
//! Archive, restore, privacy, display-name, header and purpose each end in a
//! [`App::create_system_post`]. Five are logged and swallowed, so no response body moves when one
//! fails — Go marshals the channel it read *before* the post is created.
//!
//! **The privacy post is the exception.** `UpdateChannelPrivacy` flips the type back and
//! re-updates the channel when its post fails, and answers the post's error. That rollback was
//! unreachable while there was no post to fail; it is reachable now, so it is ported. See
//! [`App::update_channel_privacy`].
//!
//! The message strings are English literals rather than untranslated ids, for the reason
//! [`App::create_system_post`] gives — with one caveat the archive and restore posts add: Go
//! builds *those two* with `i18n.GetUserTranslations(user.Locale)`, the **acting user's** locale
//! rather than the server's, so a non-English admin's archive message differs from ours.
//!
//! # What is deliberately absent
//!
//! - `ChannelAccessControlled` (ABAC) — `MinimumEnterpriseAdvancedLicense` gates it to `false`
//!   on an unlicensed installation, and the handlers forward a licensed one outright.
//! - `runGuardedChannelWillBeUpdated` / `…WillBeArchived` / `…WillBeRestored` — plugin hooks,
//!   the identity with no plugin environment, the same reading as `runGuardedDraftWillBeUpserted`.
//! - `addChannelToDefaultCategory` — a sidebar write; the handler forwards the patches that
//!   would reach it. See [`default_category_after_patch`].
//! - `cleanupChannelAccessControlPolicy` and `CancelPendingChannelJoinRequestsOnConvert` —
//!   enterprise, and Go logs rather than returns their failures.

use mm_model::channel::{CHANNEL_TYPE_OPEN, Channel, ChannelPatch, DEFAULT_CHANNEL_NAME};
use mm_model::post::{
    POST_TYPE_CHANGE_CHANNEL_PRIVACY, POST_TYPE_CHANNEL_DELETED, POST_TYPE_CHANNEL_RESTORED,
    POST_TYPE_DISPLAYNAME_CHANGE, POST_TYPE_HEADER_CHANGE, POST_TYPE_PURPOSE_CHANGE, Post,
};
use mm_model::user::User;
use mm_model::utils::{AppError, AppResult, get_millis};
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_CHANNEL_CONVERTED, WEBSOCKET_EVENT_CHANNEL_DELETED,
    WEBSOCKET_EVENT_CHANNEL_RESTORED, WEBSOCKET_EVENT_CHANNEL_UPDATED, WebSocketEvent,
};
use mm_store::{ChannelStore, PostStore, StoreError, UserStore, WebhookStore};

use crate::App;
use crate::channel_member::system_props;

/// What a channel write did, or why it declined to do it.
///
/// Same shape as [`crate::draft::DraftWrite`] and for the same reason: one branch of Go's logic
/// needs a decision this server cannot make, and the honest answer is to hand the whole request
/// to the server that can. **Every `Forward` is returned before anything is written**, so a
/// forwarded request is not a half-applied one.
#[derive(Debug)]
pub enum ChannelWrite {
    /// The channel was written and the websocket event published.
    Done,
    /// Nothing was written. Forward the request whole.
    Forward(&'static str),
}

/// `channel.DefaultCategoryName` as it will be *after* `patch` is applied — the value
/// `addChannelToDefaultCategory` (app/channel.go:4706) tests, not the value on the row now.
///
/// Go's gate is `channel.DefaultCategoryName != "" && *EnableChannelCategorySorting`, evaluated
/// on the **patched** channel, and the patch trims the name
/// ([`Channel::patch`]). So a patch sending `"  Zed  "` reaches the sidebar and one sending
/// `"   "` does not — the trim happens before the emptiness test, and a port that tested the raw
/// string would forward a request Go serves.
///
/// A free function rather than an inline expression because the answer decides whether the
/// request is served here at all, and it must be computable *before* the write.
pub fn default_category_after_patch(channel: &Channel, patch: &ChannelPatch) -> String {
    match &patch.default_category_name {
        Some(name) => name.trim().to_owned(),
        None => channel.default_category_name.clone(),
    }
}

impl App {
    /// Port of `app.App.UpdateChannel` (app/channel.go:750).
    ///
    /// **Mutates `channel` and publishes the mutated value**, because the store's `PreUpdate`
    /// mints a fresh `UpdateAt` and sanitises `Name`/`DisplayName` in place. Go's pointer
    /// semantics make this invisible; in Rust it is the difference between answering the caller's
    /// stale `update_at` and answering the one that was stored.
    ///
    /// # The re-read is not redundant
    ///
    /// Go fetches the existing row again even though every caller has just fetched it. Its only
    /// remaining observable effect here is the error: a channel that vanished between the
    /// handler's read and this one answers **404 `app.channel.get.existing.app_error`**, and any
    /// other store failure answers 500 `app.channel.get.find.app_error`. (Its other two uses —
    /// the ABAC type-conversion block and the plugin hook — are both absent, see the module
    /// docs.) `Channel().Get` filters to message channel types, so a board or space id is a 404
    /// here as well.
    ///
    /// # Four store errors, four different answers
    ///
    /// | store | id | status |
    /// |---|---|---|
    /// | unique constraint on `Name` | `store.sql_channel.save_channel.exists.app_error` | 400 |
    /// | `DeleteAt != 0` | `app.channel.update.bad_id` | 400 |
    /// | `IsValid` | the model's own `model.channel.is_valid.*` | 400 |
    /// | anything else | `app.channel.update_channel.internal_error` | 500 |
    ///
    /// The second row is why `PUT /channels/{id}/patch` on an archived channel answers
    /// `app.channel.update.bad_id`: `patchChannel` has no archived-channel guard of its own, so
    /// the store's is the one that fires. Measured against the running Go server.
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id, channel_type = %channel.channel_type))]
    pub async fn update_channel(&self, channel: &mut Channel) -> AppResult<()> {
        let params = std::collections::HashMap::from([(
            "channel_id".to_owned(),
            serde_json::Value::String(channel.id.clone()),
        )]);
        self.store()
            .channel()
            .get(&channel.id)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    AppError::boxed(
                        "UpdateChannel",
                        "app.channel.get.existing.app_error",
                        Some(params),
                        String::new(),
                        404,
                    )
                } else {
                    tracing::error!(error = %err, "channel re-read failed");
                    AppError::boxed(
                        "UpdateChannel",
                        "app.channel.get.find.app_error",
                        Some(params),
                        String::new(),
                        500,
                    )
                }
            })?;

        self.store()
            .channel()
            .update(channel)
            .await
            .map_err(update_channel_error)?;

        // `channel_updated` carries the whole channel as a **JSON string** under `channel`, not
        // as a nested object — the hub precomputes the frame and Go's `Add` takes an `any` that
        // happens to be a string here. A port that added the object would change the field's type
        // on the wire for every connected client.
        let mut message = WebSocketEvent::new(
            WEBSOCKET_EVENT_CHANNEL_UPDATED,
            "",
            &channel.id,
            "",
            None,
            "",
        );
        let encoded = serde_json::to_string(&channel).map_err(|err| {
            tracing::error!(error = %err, "failed to encode the updated channel");
            AppError::boxed(
                "UpdateChannel",
                "api.marshal_error",
                None,
                String::new(),
                500,
            )
        })?;
        message.add("channel", serde_json::Value::String(encoded));
        self.publish(message).await;

        Ok(())
    }

    /// Port of `app.App.PatchChannel` (app/channel.go:1069).
    ///
    /// Four steps in Go's order, and only the first can refuse:
    ///
    /// 1. `CheckIfChannelIsRestrictedDM` → **400
    ///    `api.channel.patch_update_channel.restricted_dm.app_error`**. It runs before the patch
    ///    is applied, so a refusal here writes nothing.
    /// 2. `channel.Patch(patch)` — see [`Channel::patch`], including the two fields it trims and
    ///    the `managed_category_name` it accepts and ignores.
    /// 3. [`App::update_channel`].
    /// 4. `addChannelToDefaultCategory`, then the display-name, header and purpose system posts —
    ///    **in that order**, each guarded on its own field having changed, each logged and
    ///    swallowed. The sidebar step is the caller's problem (it has to be decided *before*
    ///    step 3, see [`default_category_after_patch`]).
    ///
    /// The fourth post, `postUpdateChannelAutotranslationMessage`, has no call site here: an
    /// `autotranslation` patch is a **403** from the handler on an unlicensed installation, so
    /// the field cannot change.
    ///
    /// # The old values are read before `patch` is applied and compared after
    ///
    /// Go captures all three ahead of `channel.Patch(patch)` and then tests `channel.Header !=
    /// oldChannelHeader`. A patch that sets a field to the value it already had therefore writes
    /// no post, and one that omits a field cannot write one — [`Channel::patch`] only assigns
    /// what the patch carries.
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id))]
    pub async fn patch_channel(
        &self,
        channel: &mut Channel,
        patch: &ChannelPatch,
        user_id: &str,
    ) -> AppResult<ChannelWrite> {
        match self.check_if_channel_is_restricted_dm(channel).await? {
            crate::channel::RestrictedDm::No => {}
            crate::channel::RestrictedDm::Yes => {
                return Err(AppError::boxed(
                    "PatchChannel",
                    "api.channel.patch_update_channel.restricted_dm.app_error",
                    None,
                    String::new(),
                    400,
                ));
            }
            crate::channel::RestrictedDm::Undecidable => {
                return Ok(ChannelWrite::Forward(
                    "a bot's exemption from DM restrictions is a plugin decision",
                ));
            }
        }

        let old_display_name = channel.display_name.clone();
        let old_header = channel.header.clone();
        let old_purpose = channel.purpose.clone();

        channel.patch(patch);
        self.update_channel(channel).await?;

        if old_display_name != channel.display_name {
            self.post_update_channel_display_name_message(
                user_id,
                channel,
                &old_display_name,
                &channel.display_name.clone(),
            )
            .await;
        }
        if channel.header != old_header {
            self.post_update_channel_header_message(
                user_id,
                channel,
                &old_header,
                &channel.header.clone(),
            )
            .await;
        }
        if channel.purpose != old_purpose {
            self.post_update_channel_purpose_message(
                user_id,
                channel,
                &old_purpose,
                &channel.purpose.clone(),
            )
            .await;
        }

        Ok(ChannelWrite::Done)
    }

    /// Port of `app.App.UpdateChannelPrivacy` (app/channel.go:882).
    ///
    /// The caller has already set `channel.channel_type` to the requested privacy — Go's handler
    /// does that, not this function.
    ///
    /// # Converting to public clears `discoverable`, and the clearing comes first
    ///
    /// "Public channels are inherently joinable; the discoverable flag only has meaning for
    /// private channels." Cleared *before* the update rather than after, so no reader sees a
    /// public channel that is also discoverable. It also has to happen before
    /// [`Channel::is_valid`] runs inside the store, which rejects exactly that combination
    /// (`model.channel.is_valid.discoverable.app_error`) — so a port that cleared it afterwards
    /// would turn every private→public conversion of a discoverable channel into a 400.
    ///
    /// # The rollback, which the system post makes reachable
    ///
    /// If `postChannelPrivacyMessage` fails, Go flips `Type` back, restores the `discoverable`
    /// flag it eagerly cleared, re-runs `UpdateChannel` — **logging rather than returning that
    /// second update's failure** — and answers the post's error. So a failed post leaves the
    /// channel as it was and the caller sees a 500, having received no `channel_converted` event.
    /// The `channel_updated` event from the first update has already gone out and the rollback
    /// sends a second one, which is Go's behaviour and not a tidiness this port should improve.
    ///
    /// # Two events, not one
    ///
    /// `channel_updated` from [`App::update_channel`], then `channel_converted` — addressed to
    /// the **team**, carrying `channel_id` and `channel_type` as plain strings. Clients use the
    /// second to move the channel between their public and private lists; dropping it leaves
    /// every open tab showing the old privacy until it refetches.
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id, channel_type = %channel.channel_type))]
    pub async fn update_channel_privacy(
        &self,
        channel: &mut Channel,
        user: &User,
    ) -> AppResult<ChannelWrite> {
        if channel.discoverable && channel.channel_type == CHANNEL_TYPE_OPEN {
            // `CancelPendingChannelJoinRequestsOnConvert` fans out over the pending join requests
            // of a formerly discoverable private channel and broadcasts a cancellation to each
            // requester. Neither the requests table nor that broadcast is ported, and the
            // conversion must not silently strand them — so this case goes to Go whole. Only
            // reachable on a row a discoverable-channels-enabled server wrote, since the feature
            // flag is off at the pinned SHA.
            return Ok(ChannelWrite::Forward(
                "a discoverable channel's pending join requests are cancelled on convert",
            ));
        }

        let was_discoverable = channel.discoverable;
        if channel.channel_type == CHANNEL_TYPE_OPEN {
            channel.discoverable = false;
        }

        self.update_channel(channel).await?;

        if let Err(post_err) = self
            .create_system_post(channel_privacy_post(user, channel), channel)
            .await
        {
            if channel.channel_type == CHANNEL_TYPE_OPEN {
                channel.channel_type = mm_model::channel::CHANNEL_TYPE_PRIVATE.to_owned();
                channel.discoverable = was_discoverable;
            } else {
                channel.channel_type = CHANNEL_TYPE_OPEN.to_owned();
            }
            if let Err(err) = self.update_channel(channel).await {
                tracing::error!(
                    error = %err,
                    "Failed to revert channel privacy after posting an update message failed",
                );
            }
            return Err(privacy_message_error(&post_err));
        }

        let mut message = WebSocketEvent::new(
            WEBSOCKET_EVENT_CHANNEL_CONVERTED,
            &channel.team_id,
            "",
            "",
            None,
            "",
        );
        message.add("channel_id", serde_json::Value::String(channel.id.clone()));
        message.add(
            "channel_type",
            serde_json::Value::String(channel.channel_type.clone()),
        );
        self.publish(message).await;

        Ok(ChannelWrite::Done)
    }

    /// Port of `app.App.DeleteChannel` (app/channel.go:1692) — the **archive**, not the purge.
    ///
    /// # The order of the checks is not the order a reader would choose
    ///
    /// Go reads both webhook lists and the acting user *before* it asks whether the channel is
    /// already archived or is the default channel. So a webhook query failure is a 500 for a
    /// channel that would otherwise have answered 400, and the acting user's absence is a 404
    /// ahead of both. Reproduced in Go's order.
    ///
    /// Then, in this order: `DeleteAt > 0` → 400
    /// `api.channel.delete_channel.deleted.app_error`; `Name == "town-square"` → 400
    /// `api.channel.delete_channel.cannot.app_error` with `Channel` in its params. Both errors are
    /// raised under the id **`deleteChannel`** — the handler's name, not `DeleteChannel` — which
    /// is Go's inconsistency and is not on the wire, but is what a reader checking the source
    /// against this will see.
    ///
    /// # Archiving a channel archives its webhooks, and those failures are swallowed
    ///
    /// Every live incoming and outgoing hook on the channel is soft-deleted at one shared
    /// timestamp, taken **after** the channel's own `DeleteAt` — two `GetMillis()` calls, so the
    /// two are not equal. Each individual failure is logged and the loop continues, so a broken
    /// hook delete does not stop the archive.
    ///
    /// # Returns the `delete_at` it wrote
    ///
    /// The websocket event carries it and the handler's body does not, so it is a return value
    /// rather than a field on the channel the caller holds.
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id, channel_type = %channel.channel_type))]
    pub async fn delete_channel(&self, channel: &Channel, user_id: &str) -> AppResult<i64> {
        // Go runs the two webhook reads in goroutines and joins them below; sequential here,
        // because the join order is what decides which error wins and concurrency would make it
        // a race. Go's join order is incoming, then outgoing.
        let incoming = self.store().webhook().get_incoming_by_channel(&channel.id);
        let outgoing = self.store().webhook().get_outgoing_by_channel(&channel.id);

        let user = if user_id.is_empty() {
            None
        } else {
            Some(self.store().user().get(user_id).await.map_err(|err| {
                if err.is_not_found() {
                    AppError::boxed(
                        "DeleteChannel",
                        "app.user.missing_account.const",
                        None,
                        String::new(),
                        404,
                    )
                } else {
                    tracing::error!(error = %err, "the archiving user's lookup failed");
                    AppError::boxed(
                        "DeleteChannel",
                        "app.user.get.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })?)
        };

        let incoming = incoming.await.map_err(|err| {
            tracing::error!(error = %err, "incoming webhook lookup failed");
            AppError::boxed(
                "DeleteChannel",
                "app.webhooks.get_incoming_by_channel.app_error",
                None,
                String::new(),
                500,
            )
        })?;
        let outgoing = outgoing.await.map_err(|err| {
            tracing::error!(error = %err, "outgoing webhook lookup failed");
            AppError::boxed(
                "DeleteChannel",
                "app.webhooks.get_outgoing_by_channel.app_error",
                None,
                String::new(),
                500,
            )
        })?;

        if channel.delete_at > 0 {
            return Err(AppError::boxed(
                "deleteChannel",
                "api.channel.delete_channel.deleted.app_error",
                None,
                String::new(),
                400,
            ));
        }

        if channel.name == DEFAULT_CHANNEL_NAME {
            return Err(AppError::boxed(
                "deleteChannel",
                "api.channel.delete_channel.cannot.app_error",
                Some(std::collections::HashMap::from([(
                    "Channel".to_owned(),
                    serde_json::Value::String(DEFAULT_CHANNEL_NAME.to_owned()),
                )])),
                String::new(),
                400,
            ));
        }

        let delete_at = get_millis();
        self.store()
            .channel()
            .delete(&channel.id, delete_at)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "channel archive failed");
                AppError::boxed(
                    "DeleteChannel",
                    "app.channel.delete.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        // The archive post comes **before** the webhook cleanup, not after it — so a channel whose
        // archive post fails still loses its webhooks. `if channel.IsSpace()` guards it: a space's
        // backing channel is archived silently.
        if !channel.is_space() {
            match &user {
                Some(user) => {
                    self.post_system_message(channel_deleted_post(user, &channel.id), channel)
                        .await;
                }
                // Go's `else` posts as the system bot, which `GetSystemBot` would create. Not
                // reachable from `DELETE /channels/{id}` — the handler always has a session.
                None => tracing::warn!(
                    channel_id = %channel.id,
                    "Failed to post archive message: GetSystemBot is not ported",
                ),
            }
        }

        // A second `GetMillis()`: the hooks' `DeleteAt` is later than the channel's.
        let hook_deleted_at = get_millis();
        for hook in &incoming {
            if let Err(err) = self
                .store()
                .webhook()
                .delete_incoming(&hook.id, hook_deleted_at)
                .await
            {
                tracing::warn!(hook_id = %hook.id, error = %err, "Encountered error deleting incoming webhook");
            }
        }
        for hook in &outgoing {
            if let Err(err) = self
                .store()
                .webhook()
                .delete_outgoing(&hook.id, hook_deleted_at)
                .await
            {
                tracing::warn!(hook_id = %hook.id, error = %err, "Encountered error deleting outgoing webhook");
            }
        }

        // The one cleanup on this path Go does **not** swallow.
        self.store()
            .post()
            .delete_persistent_notifications_by_channel(&channel.id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "retiring the channel's persistent notifications failed");
                AppError::boxed(
                    "DeleteChannel",
                    "app.post_persistent_notification.delete_by_channel.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        let mut message = if channel.channel_type == CHANNEL_TYPE_OPEN {
            WebSocketEvent::new(
                WEBSOCKET_EVENT_CHANNEL_DELETED,
                &channel.team_id,
                "",
                "",
                None,
                "",
            )
        } else {
            WebSocketEvent::new(
                WEBSOCKET_EVENT_CHANNEL_DELETED,
                "",
                &channel.id,
                "",
                None,
                "",
            )
        };
        message.add("channel_id", serde_json::Value::String(channel.id.clone()));
        // A JSON **number**, not a string — `Add` takes Go's `int64` here where `channel_id` is a
        // `string`, and the two are not interchangeable to a client parsing the frame.
        message.add("delete_at", serde_json::Value::from(delete_at));
        self.publish(message).await;

        Ok(delete_at)
    }

    /// Port of `app.App.RestoreChannel` (app/channel.go:979) — the inverse of
    /// [`App::delete_channel`], with one ordering that looks like a bug and is reproduced anyway.
    ///
    /// # The acting user is looked up *after* the channel is restored and the event is published
    ///
    /// Go's sequence is: refuse a channel that is not archived (400
    /// `api.channel.restore_channel.restored.app_error`), restore it, zero `DeleteAt`, publish
    /// `channel_restored`, **then** load the user for the unarchive system post — and a failure
    /// there is returned to the client as a 404 or 500. So a caller whose own user row is missing
    /// gets an error for a request that succeeded, and every connected client has already been
    /// told. Reproduced: the lookup is a real query on a real path, and moving it earlier would
    /// turn that error into one raised before the write.
    ///
    /// The unarchive post is the only reason the lookup exists, which is exactly why a port that
    /// dropped it would look tidier and answer differently.
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id, channel_type = %channel.channel_type))]
    pub async fn restore_channel(&self, channel: &mut Channel, user_id: &str) -> AppResult<()> {
        if channel.delete_at == 0 {
            return Err(AppError::boxed(
                "restoreChannel",
                "api.channel.restore_channel.restored.app_error",
                None,
                String::new(),
                400,
            ));
        }

        self.store()
            .channel()
            .restore(&channel.id, get_millis())
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "channel restore failed");
                AppError::boxed(
                    "RestoreChannel",
                    "app.channel.restore.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        channel.delete_at = 0;

        let mut message = if channel.channel_type == CHANNEL_TYPE_OPEN {
            WebSocketEvent::new(
                WEBSOCKET_EVENT_CHANNEL_RESTORED,
                &channel.team_id,
                "",
                "",
                None,
                "",
            )
        } else {
            WebSocketEvent::new(
                WEBSOCKET_EVENT_CHANNEL_RESTORED,
                "",
                &channel.id,
                "",
                None,
                "",
            )
        };
        message.add("channel_id", serde_json::Value::String(channel.id.clone()));
        self.publish(message).await;

        if !user_id.is_empty() {
            let user = self.store().user().get(user_id).await.map_err(|err| {
                if err.is_not_found() {
                    AppError::boxed(
                        "RestoreChannel",
                        "app.user.missing_account.const",
                        None,
                        String::new(),
                        404,
                    )
                } else {
                    tracing::error!(error = %err, "the restoring user's lookup failed");
                    AppError::boxed(
                        "RestoreChannel",
                        "app.user.get.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })?;

            self.post_system_message(channel_restored_post(&user, &channel.id), channel)
                .await;
        }

        Ok(())
    }

    /// Port of `app.App.PostUpdateChannelDisplayNameMessage` (app/channel.go:2198).
    ///
    /// # One sentence, three props, and no "removed" variant
    ///
    /// Unlike the header and purpose notices this has a single message form — Go uses
    /// `…updated_from` whatever the old and new values are, so clearing a display name reads
    /// "updated the channel display name from: Old to: ".
    ///
    /// # It is called from two places with two different "new" values
    ///
    /// [`App::patch_channel`] passes the **patched** channel's display name. `updateChannel`'s
    /// handler passes the **submitted** one, which is not the same thing: a body that omits
    /// `display_name` leaves the channel's unchanged and still posts, with an empty new value.
    /// See `mm_api::channel_writes::update_channel`.
    ///
    /// The user lookup is this function's, and its failure is a 400 in Go — swallowed by every
    /// caller, so it is logged here.
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id, user_id = %user_id))]
    pub async fn post_update_channel_display_name_message(
        &self,
        user_id: &str,
        channel: &Channel,
        old_display_name: &str,
        new_display_name: &str,
    ) {
        let Some(user) = self
            .system_post_author(user_id, "channel display name")
            .await
        else {
            return;
        };

        let post = Post {
            channel_id: channel.id.clone(),
            message: format!(
                "{} updated the channel display name from: {} to: {}",
                user.username, old_display_name, new_display_name
            ),
            post_type: POST_TYPE_DISPLAYNAME_CHANGE.to_owned(),
            user_id: user_id.to_owned(),
            props: Some(system_props([
                ("username", user.username.as_str()),
                ("old_displayname", old_display_name),
                ("new_displayname", new_display_name),
            ])),
            ..Post::default()
        };

        self.post_system_message(post, channel).await;
    }

    /// Port of `app.App.PostUpdateChannelHeaderMessage` (app/channel.go:2100).
    ///
    /// Three sentences, chosen by which side is empty, and the empty-old test comes **first** —
    /// so setting a header on a channel that had none reads "updated … to:", and clearing one
    /// reads "removed … (was:". A reader who swapped the two branches would get both halves of
    /// every pair backwards.
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id, user_id = %user_id))]
    pub async fn post_update_channel_header_message(
        &self,
        user_id: &str,
        channel: &Channel,
        old_header: &str,
        new_header: &str,
    ) {
        let Some(user) = self.system_post_author(user_id, "channel header").await else {
            return;
        };

        let message = if old_header.is_empty() {
            format!(
                "{} updated the channel header to: {}",
                user.username, new_header
            )
        } else if new_header.is_empty() {
            format!(
                "{} removed the channel header (was: {})",
                user.username, old_header
            )
        } else {
            format!(
                "{} updated the channel header from: {} to: {}",
                user.username, old_header, new_header
            )
        };

        let post = Post {
            channel_id: channel.id.clone(),
            message,
            post_type: POST_TYPE_HEADER_CHANGE.to_owned(),
            user_id: user_id.to_owned(),
            props: Some(system_props([
                ("username", user.username.as_str()),
                ("old_header", old_header),
                ("new_header", new_header),
            ])),
            ..Post::default()
        };

        self.post_system_message(post, channel).await;
    }

    /// Port of `app.App.PostUpdateChannelPurposeMessage` (app/channel.go:2134).
    ///
    /// The same three-way shape as the header notice with one difference that is not cosmetic:
    /// its i18n ids live under **`app.channel.`** where the header's live under `api.channel.`,
    /// and the error it raises on a missing user is `app.channel.…retrieve_user.error` rather
    /// than `api.channel.…retrieve_user.error`. Neither reaches the wire, both are swallowed.
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id, user_id = %user_id))]
    pub async fn post_update_channel_purpose_message(
        &self,
        user_id: &str,
        channel: &Channel,
        old_purpose: &str,
        new_purpose: &str,
    ) {
        let Some(user) = self.system_post_author(user_id, "channel purpose").await else {
            return;
        };

        let message = if old_purpose.is_empty() {
            format!(
                "{} updated the channel purpose to: {}",
                user.username, new_purpose
            )
        } else if new_purpose.is_empty() {
            format!(
                "{} removed the channel purpose (was: {})",
                user.username, old_purpose
            )
        } else {
            format!(
                "{} updated the channel purpose from: {} to: {}",
                user.username, old_purpose, new_purpose
            )
        };

        let post = Post {
            channel_id: channel.id.clone(),
            message,
            post_type: POST_TYPE_PURPOSE_CHANGE.to_owned(),
            user_id: user_id.to_owned(),
            props: Some(system_props([
                ("username", user.username.as_str()),
                ("old_purpose", old_purpose),
                ("new_purpose", new_purpose),
            ])),
            ..Post::default()
        };

        self.post_system_message(post, channel).await;
    }

    /// The `Store().User().Get(userID)` the three "the channel changed" notices each open with.
    ///
    /// Go turns its failure into an `AppError` that every caller logs and discards, so the
    /// post simply does not happen. Returning `None` keeps that in one place.
    async fn system_post_author(&self, user_id: &str, what: &str) -> Option<User> {
        match self.store().user().get(user_id).await {
            Ok(user) => Some(user),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    user_id = %user_id,
                    "Error while posting {what} message",
                );
                None
            }
        }
    }

    /// The acting user for `updateChannelPrivacy`, which Go fetches through `App.GetUser` — so
    /// its 404 is `MissingAccountError` and it is raised **before** the conversion.
    ///
    /// Only the username reaches the (unported) system post, so nothing in the response depends
    /// on it; the lookup is here because its error is on the wire.
    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    pub async fn privacy_change_author(&self, user_id: &str) -> AppResult<User> {
        self.get_user(user_id).await
    }
}

/// The `model.Post` literal of `postChannelPrivacyMessage` (app/channel.go:963).
///
/// # The sentence is picked by the **new** type and says nothing about who changed it
///
/// A map literal indexed by `channel.Type`, so the public sentence is the one a channel that is
/// now open gets. Neither string interpolates the username, which only reaches `props`.
fn channel_privacy_post(user: &User, channel: &Channel) -> Post {
    let message = if channel.channel_type == CHANNEL_TYPE_OPEN {
        "This channel has been converted to a Public Channel and can be joined by any team member."
    } else {
        "This channel has been converted to a Private Channel."
    };

    Post {
        channel_id: channel.id.clone(),
        message: message.to_owned(),
        post_type: POST_TYPE_CHANGE_CHANNEL_PRIVACY.to_owned(),
        user_id: user.id.clone(),
        props: Some(system_props([("username", user.username.as_str())])),
        ..Post::default()
    }
}

/// The `model.Post` literal of `App.DeleteChannel`'s archive notice (app/channel.go:1768).
fn channel_deleted_post(user: &User, channel_id: &str) -> Post {
    Post {
        channel_id: channel_id.to_owned(),
        message: format!("{} archived the channel.", user.username),
        post_type: POST_TYPE_CHANNEL_DELETED.to_owned(),
        user_id: user.id.clone(),
        props: Some(system_props([("username", user.username.as_str())])),
        ..Post::default()
    }
}

/// The `model.Post` literal of `App.RestoreChannel`'s unarchive notice (app/channel.go:1029).
///
/// Its i18n string is the one **named-parameter** template among the twelve
/// (`{{.Username}} unarchived the channel.`) where the others are `%v`. The rendered sentence is
/// the same shape; the difference is only visible in `en.json`.
fn channel_restored_post(user: &User, channel_id: &str) -> Post {
    Post {
        channel_id: channel_id.to_owned(),
        message: format!("{} unarchived the channel.", user.username),
        post_type: POST_TYPE_CHANNEL_RESTORED.to_owned(),
        user_id: user.id.clone(),
        props: Some(system_props([("username", user.username.as_str())])),
        ..Post::default()
    }
}

/// `NewAppError("postChannelPrivacyMessage", "api.channel.post_channel_privacy_message.error",
/// nil, "", 500)` — the wrap `UpdateChannelPrivacy` returns to the client after it rolls back.
///
/// A different id from the membership posts' shared
/// `api.channel.post_user_add_remove_message_and_forget.error`, and this is the **only** one of
/// the six lifecycle posts whose id can reach a response body.
fn privacy_message_error(cause: &AppError) -> Box<AppError> {
    tracing::error!(error = %cause, "the channel privacy system post failed");
    AppError::boxed(
        "postChannelPrivacyMessage",
        "api.channel.post_channel_privacy_message.error",
        None,
        String::new(),
        500,
    )
}

/// Go's four-arm `switch` on the store error in `App.UpdateChannel` (app/channel.go:806-820).
///
/// The arms are tried in Go's order and the order is observable: a `Conflict` is *also* a store
/// error, so putting the default first would turn every duplicate channel name into a 500.
fn update_channel_error(err: StoreError) -> Box<AppError> {
    if err.conflict_resource() == Some("Name") {
        return AppError::boxed(
            "UpdateChannel",
            // `store.ChannelExistsError` (store/constants.go:7) — the *save* id, reused by
            // update. It reads wrong and it is what Go sends.
            "store.sql_channel.save_channel.exists.app_error",
            None,
            String::new(),
            400,
        );
    }
    if err.is_invalid_input() {
        return AppError::boxed(
            "UpdateChannel",
            "app.channel.update.bad_id",
            None,
            String::new(),
            400,
        );
    }
    if let StoreError::Invalid { app_error, .. } = err {
        // `IsValid` runs inside the store, so the model's own error reaches the client unwrapped
        // — `model.channel.is_valid.name.app_error` and friends, not a store id.
        return app_error;
    }
    tracing::error!(error = %err, "channel update failed");
    AppError::boxed(
        "UpdateChannel",
        "app.channel.update_channel.internal_error",
        None,
        String::new(),
        500,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acting_user() -> User {
        User {
            id: "uuuuuuuuuuuuuuuuuuuuuuuua".to_owned(),
            username: "alice".to_owned(),
            ..User::default()
        }
    }

    fn props_of(post: &Post) -> Vec<(String, String)> {
        post.get_props()
            .into_iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("<not a string>").to_owned()))
            .collect()
    }

    fn typed(channel_type: &str) -> Channel {
        Channel {
            id: "cccccccccccccccccccccccc1".to_owned(),
            channel_type: channel_type.to_owned(),
            ..Channel::default()
        }
    }

    /// The privacy notice's sentence is picked by the type the channel **now** has, and neither
    /// sentence names the user. Swapping the two arms tells every client in the channel the
    /// opposite of what happened.
    #[test]
    fn the_privacy_post_sentence_follows_the_new_type() {
        let user = acting_user();

        let to_public = channel_privacy_post(&user, &typed(CHANNEL_TYPE_OPEN));
        assert_eq!(to_public.post_type, "system_change_chan_privacy");
        assert_eq!(
            to_public.message,
            "This channel has been converted to a Public Channel and can be joined by any team \
             member."
        );
        assert_eq!(
            props_of(&to_public),
            vec![("username".to_owned(), "alice".to_owned())]
        );

        let to_private =
            channel_privacy_post(&user, &typed(mm_model::channel::CHANNEL_TYPE_PRIVATE));
        assert_eq!(
            to_private.message,
            "This channel has been converted to a Private Channel."
        );
        assert_eq!(to_private.user_id, user.id);
    }

    /// Archive and restore differ by one word and one type constant, and both count towards the
    /// channel's message total — neither is a join/leave message.
    #[test]
    fn the_archive_and_restore_posts_are_counted_messages() {
        let user = acting_user();

        let archived = channel_deleted_post(&user, "c1");
        assert_eq!(archived.post_type, "system_channel_deleted");
        assert_eq!(archived.message, "alice archived the channel.");
        assert!(!archived.excludes_from_channel_message_count());

        let restored = channel_restored_post(&user, "c1");
        assert_eq!(restored.post_type, "system_channel_restored");
        assert_eq!(restored.message, "alice unarchived the channel.");
        assert!(!restored.excludes_from_channel_message_count());

        assert_eq!(
            props_of(&restored),
            vec![("username".to_owned(), "alice".to_owned())]
        );
    }

    /// The one id that can reach a client from any of the six lifecycle posts.
    #[test]
    fn the_privacy_post_failure_has_its_own_id() {
        let cause = AppError::new(
            "CreatePost",
            "app.post.save.app_error",
            None,
            String::new(),
            500,
        );
        let wrapped = privacy_message_error(&cause);
        assert_eq!(wrapped.id, "api.channel.post_channel_privacy_message.error");
        assert_eq!(wrapped.status_code, 500);
        assert_eq!(wrapped.where_, "postChannelPrivacyMessage");
    }

    fn channel_with_category(name: &str) -> Channel {
        Channel {
            default_category_name: name.to_owned(),
            ..Channel::default()
        }
    }

    /// The gate reads the **patched** value, and the patch trims. A test on the raw string passes
    /// for `"Zed"` and forwards nothing for `"  Zed  "`.
    #[test]
    fn the_default_category_gate_reads_the_trimmed_patched_value() {
        let channel = channel_with_category("");
        let patch = ChannelPatch {
            default_category_name: Some("  Zed  ".to_owned()),
            ..ChannelPatch::default()
        };
        assert_eq!(default_category_after_patch(&channel, &patch), "Zed");

        let whitespace = ChannelPatch {
            default_category_name: Some("   ".to_owned()),
            ..ChannelPatch::default()
        };
        assert_eq!(
            default_category_after_patch(&channel, &whitespace),
            "",
            "a whitespace-only name trims to empty and never reaches the sidebar"
        );
    }

    /// A patch that does not mention the field keeps whatever the row already carries — so a
    /// channel already in a default category forwards even for a header-only patch.
    #[test]
    fn an_untouched_default_category_still_counts() {
        let channel = channel_with_category("Zed");
        let header_only = ChannelPatch {
            header: Some("hi".to_owned()),
            ..ChannelPatch::default()
        };
        assert_eq!(default_category_after_patch(&channel, &header_only), "Zed");

        // And clearing it explicitly takes the request out of the sidebar path.
        let cleared = ChannelPatch {
            default_category_name: Some(String::new()),
            ..ChannelPatch::default()
        };
        assert_eq!(default_category_after_patch(&channel, &cleared), "");
    }

    /// The four arms, in Go's order, each keeping its own id and status. `Conflict` first: it is
    /// also a store error, and an arm order that fell through would answer 500 for a name clash.
    #[test]
    fn the_store_error_arms_keep_gos_ids_and_statuses() {
        let conflict = update_channel_error(StoreError::Conflict {
            resource: "Name",
            source: sqlx::Error::RowNotFound,
        });
        assert_eq!(
            conflict.id,
            "store.sql_channel.save_channel.exists.app_error"
        );
        assert_eq!(conflict.status_code, 400);

        let archived = update_channel_error(StoreError::InvalidInput {
            entity: "Channel",
            field: "DeleteAt",
            value: "1".to_owned(),
        });
        assert_eq!(archived.id, "app.channel.update.bad_id");
        assert_eq!(archived.status_code, 400);

        let invalid = update_channel_error(StoreError::Invalid {
            entity: "Channel",
            app_error: AppError::boxed(
                "Channel.IsValid",
                "model.channel.is_valid.name.app_error",
                None,
                String::new(),
                400,
            ),
        });
        assert_eq!(
            invalid.id, "model.channel.is_valid.name.app_error",
            "the model's error reaches the client unwrapped"
        );

        let broken = update_channel_error(StoreError::Db {
            context: "boom".to_owned(),
            source: sqlx::Error::RowNotFound,
        });
        assert_eq!(broken.id, "app.channel.update_channel.internal_error");
        assert_eq!(broken.status_code, 500);
    }

    /// A `Conflict` on some other resource is not a name clash. Go matches the constraint by
    /// name, so a widened match would report a duplicate channel name for an unrelated violation.
    #[test]
    fn only_a_name_conflict_is_the_exists_error() {
        let other = update_channel_error(StoreError::Conflict {
            resource: "Email",
            source: sqlx::Error::RowNotFound,
        });
        assert_eq!(other.id, "app.channel.update_channel.internal_error");
        assert_eq!(other.status_code, 500);
    }
}
