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
//! # The system posts are not written here
//!
//! Go's delete, restore, privacy, display-name, header and purpose paths each create a system
//! post, and logs-and-swallows every failure. Post writes are not ported yet, so those posts are
//! missing — a recorded gap ([D-232]), not a silent one. Nothing in any of these five response
//! bodies depends on them, because Go marshals the channel it read *before* the post is created.
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
use mm_model::user::User;
use mm_model::utils::{AppError, AppResult, get_millis};
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_CHANNEL_CONVERTED, WEBSOCKET_EVENT_CHANNEL_DELETED,
    WEBSOCKET_EVENT_CHANNEL_RESTORED, WEBSOCKET_EVENT_CHANNEL_UPDATED, WebSocketEvent,
};
use mm_store::{ChannelStore, StoreError, UserStore, WebhookStore};

use crate::App;

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
    /// 4. `addChannelToDefaultCategory`, then four "the display name / header / purpose /
    ///    autotranslation changed" system posts. The sidebar step is the caller's problem (it has
    ///    to be decided *before* step 3, see [`default_category_after_patch`]) and the posts are
    ///    [D-232].
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id))]
    pub async fn patch_channel(
        &self,
        channel: &mut Channel,
        patch: &ChannelPatch,
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

        channel.patch(patch);
        self.update_channel(channel).await?;
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
    /// # Go's rollback is unreachable here, and that is the whole reason the post matters
    ///
    /// If `postChannelPrivacyMessage` fails, Go flips the type back and re-updates. This port
    /// creates no post, so it never rolls back — which is *closer* to the successful path than a
    /// port that invented a failure would be, but it does mean a Go-side post failure and ours
    /// diverge. Recorded with the rest of [D-232].
    ///
    /// # Two events, not one
    ///
    /// `channel_updated` from [`App::update_channel`], then `channel_converted` — addressed to
    /// the **team**, carrying `channel_id` and `channel_type` as plain strings. Clients use the
    /// second to move the channel between their public and private lists; dropping it leaves
    /// every open tab showing the old privacy until it refetches.
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id, channel_type = %channel.channel_type))]
    pub async fn update_channel_privacy(&self, channel: &mut Channel) -> AppResult<ChannelWrite> {
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

        if channel.channel_type == CHANNEL_TYPE_OPEN {
            channel.discoverable = false;
        }

        self.update_channel(channel).await?;

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

        if !user_id.is_empty() {
            self.store().user().get(user_id).await.map_err(|err| {
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
            })?;
        }

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

        // `PostPersistentNotification().DeleteByChannel` is not ported — [D-233]. Go answers 500
        // when it fails; there is nothing here to fail, and the rows it would have retired stay
        // live for a job that does not run on this side either.

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
    /// The post itself is [D-232]. It is the only reason the lookup exists, which is exactly why
    /// a port that dropped it would look tidier and answer differently.
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
            self.store().user().get(user_id).await.map_err(|err| {
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
        }

        Ok(())
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
