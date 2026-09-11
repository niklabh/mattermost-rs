//! Port of the "mark a channel read" half of `channels/app/channel.go`: `ViewChannel` (:3737),
//! `MarkChannelsAsViewed` (:3678), `SetActiveChannel` (:3158) and `IsCRTEnabledForUser` (:3183).
//!
//! # What this path writes, and what it only announces
//!
//! One statement writes: `UpdateLastViewedAt` zeroes the three mention counters and pulls
//! `MsgCount`/`MsgCountRoot`/`LastViewedAt` forward. Optionally a second one does —
//! `Thread().MarkAllAsReadByChannels`, and only when the client says it does *not* render
//! collapsed threads itself. Everything else on the path is announcement: a websocket event per
//! configuration flag, and a push-notification clear this port does not have (see below).
//!
//! # `clearPushNotification` is not ported
//!
//! Go queues a `notificationTypeClear` on `Srv().PushNotificationsHub` for each channel in
//! `channelsToClearPushNotifications` (notification_push.go:406). There is no push hub in this
//! port and no device to clear, so the list is computed — the store query that produces it is
//! ported in full, because getting its notify-prop fall-through wrong would be invisible until
//! there *is* a hub — and then dropped. Nothing about it reaches the HTTP response. [D-215].

use mm_model::config::{COLLAPSED_THREADS_ALWAYS_ON, COLLAPSED_THREADS_DISABLED};
use mm_model::preference::{
    PREFERENCE_CATEGORY_DISPLAY_SETTINGS, PREFERENCE_NAME_COLLAPSED_THREADS_ENABLED,
};
use mm_model::status::{STATUS_OFFLINE, STATUS_ONLINE, Status};
use mm_model::utils::{AppError, AppResult, get_millis};
use mm_store::{ChannelStore, PreferenceStore, ThreadStore, UserStore};
use std::collections::BTreeMap;

use crate::App;

impl App {
    /// Port of `App.IsCRTEnabledForUser` (channel.go:3183).
    ///
    /// **Two of the four settings never read the preference.** `disabled` is false and
    /// `always_on` — the shipped default — is true, both without a query. Only `default_on` and
    /// `default_off` consult `display_settings/collapsed_reply_threads`, and there the
    /// *preference* decides outright: `value == "on"`, so any other value (including `"off"` and
    /// including a value the client never wrote) is false. A failed lookup is not an error; the
    /// setting's own default stands.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, setting))]
    pub async fn is_crt_enabled_for_user(&self, user_id: &str) -> bool {
        let app_crt = &self.config().collapsed_threads;
        tracing::Span::current().record("setting", app_crt.as_str());

        if app_crt == COLLAPSED_THREADS_DISABLED {
            return false;
        }
        if app_crt == COLLAPSED_THREADS_ALWAYS_ON {
            return true;
        }

        let mut threads_enabled = app_crt == mm_model::config::COLLAPSED_THREADS_DEFAULT_ON;
        if let Ok(preference) = self
            .store()
            .preference()
            .get(
                user_id,
                PREFERENCE_CATEGORY_DISPLAY_SETTINGS,
                PREFERENCE_NAME_COLLAPSED_THREADS_ENABLED,
            )
            .await
        {
            threads_enabled = preference.value == "on";
        }
        threads_enabled
    }

    /// Port of `App.SetActiveChannel` (channel.go:3158).
    ///
    /// **It writes nothing to the database.** The status goes into the in-process cache and, when
    /// the status *string* changed, onto the websocket — `SaveAndBroadcastStatus` is not used, so
    /// the `Status` row keeps whatever it held. A port that persisted here would make the two
    /// servers disagree about a column Go leaves alone.
    ///
    /// Three details in the miss branch are easy to lose: a user with no status at all is
    /// invented as **online** (not offline) with `Manual: false`, the `oldStatus` it is compared
    /// against is `offline`, so the broadcast always fires; and on the hit branch the promotion to
    /// online happens only when the status is **not** manual *and* the channel id is non-empty —
    /// so losing focus (`channel_id: ""`) never revives a user's status.
    ///
    /// Go's signature returns `*model.AppError` and the body has no path that produces one; the
    /// `ViewChannel` caller propagates it regardless. Ported as an infallible call for that
    /// reason — there is no error to forward.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, channel_id = %channel_id))]
    pub async fn set_active_channel(&self, user_id: &str, channel_id: &str) {
        let mut old_status = STATUS_OFFLINE.to_owned();

        let status = match self.get_status(user_id).await {
            Ok(status) if !status.user_id.is_empty() => {
                old_status = status.status.clone();
                let mut status = status;
                status.active_channel = channel_id.to_owned();
                if !status.manual && !channel_id.is_empty() {
                    status.status = STATUS_ONLINE.to_owned();
                }
                status.last_activity_at = get_millis();
                status
            }
            _ => Status {
                user_id: user_id.to_owned(),
                status: STATUS_ONLINE.to_owned(),
                manual: false,
                last_activity_at: get_millis(),
                active_channel: channel_id.to_owned(),
                ..Status::default()
            },
        };

        self.add_status_cache(&status);

        if status.status != old_status {
            self.broadcast_status(&status).await;
        }
    }

    /// Port of `App.MarkChannelsAsViewed` (channel.go:3678).
    ///
    /// # The early return is before the thread write, not after it
    ///
    /// With nothing unread, Go returns `times` — the full read-time map, including the channels
    /// it did *not* touch — and skips the thread update, the channel update and both events. So
    /// viewing an already-read channel is a 200 with a populated body and zero writes.
    ///
    /// # `updateThreads` is three flags, and the client supplies one of them
    ///
    /// `ThreadAutoFollow && (!collapsedThreadsSupported || !isCRTEnabled)`. The client's
    /// `collapsed_threads_supported` is the only half a request can move: a client that renders
    /// threads itself marks its own thread memberships read and asks the server not to.
    ///
    /// # The two events are gated differently
    ///
    /// `multiple_channels_viewed` is gated on `EnableChannelViewedMessages` and carries
    /// `channel_times` — the whole map, not just the channels that changed.
    /// `thread_read_changed` is gated on `updateThreads && isCRTEnabled`, which with the shipped
    /// `always_on` default can only be true when the client said it does **not** support
    /// collapsed threads, and is published **once per channel** with one shared timestamp.
    #[tracing::instrument(skip(self, channel_ids), fields(user_id = %user_id, channels = channel_ids.len(), viewed))]
    pub async fn mark_channels_as_viewed(
        &self,
        channel_ids: &[String],
        user_id: &str,
        collapsed_threads_supported: bool,
        is_crt_enabled: bool,
    ) -> AppResult<BTreeMap<String, i64>> {
        let user = self.store().user().get(user_id).await.map_err(|err| {
            tracing::error!(error = %err, "user lookup failed");
            AppError::boxed(
                "MarkChannelsAsViewed",
                "app.user.get.app_error",
                None,
                String::new(),
                500,
            )
        })?;

        let unreads = self
            .store()
            .channel()
            .get_channels_with_unreads_and_with_mentions(
                channel_ids,
                user_id,
                user.notify_props.as_ref(),
            )
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "unreads-and-mentions lookup failed");
                AppError::boxed(
                    "MarkChannelsAsViewed",
                    "app.channel.get_channels_with_unreads_and_with_mentions.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        tracing::Span::current().record("viewed", unreads.with_unreads.len());

        if unreads.with_unreads.is_empty() {
            return Ok(unreads.read_times);
        }

        let update_threads =
            self.config().thread_auto_follow && (!collapsed_threads_supported || !is_crt_enabled);
        if update_threads {
            self.store()
                .thread()
                .mark_all_as_read_by_channels(user_id, &unreads.with_unreads)
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, "thread mark-read failed");
                    AppError::boxed(
                        "MarkChannelsAsViewed",
                        "app.thread.mark_all_as_read_by_channels.app_error",
                        None,
                        String::new(),
                        500,
                    )
                })?;
        }

        self.store()
            .channel()
            .update_last_viewed_at(&unreads.with_unreads, user_id)
            .await
            .map_err(update_last_viewed_at_error)?;

        if self.config().enable_channel_viewed_messages {
            let mut message = mm_model::websocket_message::WebSocketEvent::new(
                mm_model::websocket_message::WEBSOCKET_EVENT_MULTIPLE_CHANNELS_VIEWED,
                "",
                "",
                user_id,
                None,
                "",
            );
            message.add(
                "channel_times",
                serde_json::to_value(&unreads.read_times).unwrap_or(serde_json::Value::Null),
            );
            self.publish(message).await;
        }

        if update_threads && is_crt_enabled {
            let timestamp = get_millis();
            for channel_id in &unreads.with_unreads {
                let mut message = mm_model::websocket_message::WebSocketEvent::new(
                    mm_model::websocket_message::WEBSOCKET_EVENT_THREAD_READ_CHANGED,
                    "",
                    channel_id,
                    user_id,
                    None,
                    "",
                );
                message.add("timestamp", serde_json::Value::from(timestamp));
                self.publish(message).await;
            }
        }

        Ok(unreads.read_times)
    }

    /// Port of `App.ViewChannel` (channel.go:3737).
    ///
    /// **`SetActiveChannel` runs first and runs unconditionally** — before the ids are collected,
    /// so a request with both ids blank still updates the cached status (to "no active channel")
    /// and can still broadcast a status change. Only after that does the empty-id case return an
    /// **empty map**, which is a `{}` on the wire and not a null.
    ///
    /// The id order is `channel_id` then `prev_channel_id`; both are optional and either may be
    /// blank. Blank means "focus loss or initial view", which is why neither is validated as an
    /// id here — the handler does that, and only for the non-blank ones.
    #[tracing::instrument(skip(self), fields(user_id = %user_id))]
    pub async fn view_channel(
        &self,
        view: &mm_model::channel_view::ChannelView,
        user_id: &str,
        collapsed_threads_supported: bool,
    ) -> AppResult<BTreeMap<String, i64>> {
        self.set_active_channel(user_id, &view.channel_id).await;

        let mut channel_ids = Vec::new();
        if !view.channel_id.is_empty() {
            channel_ids.push(view.channel_id.clone());
        }
        if !view.prev_channel_id.is_empty() {
            channel_ids.push(view.prev_channel_id.clone());
        }

        if channel_ids.is_empty() {
            return Ok(BTreeMap::new());
        }

        let is_crt_enabled = self.is_crt_enabled_for_user(user_id).await;
        self.mark_channels_as_viewed(
            &channel_ids,
            user_id,
            collapsed_threads_supported,
            is_crt_enabled,
        )
        .await
    }

    /// Port of `App.MarkTeamChannelsAndThreadsViewed` (channel.go:3555) — the whole of one team
    /// marked read, behind `PUT /users/{user_id}/teams/{team_id}/read`.
    ///
    /// # It is `MarkChannelsAsViewed` with three deliberate differences
    ///
    /// - **The thread store gets every channel, not the unread ones.** `times` covers every
    ///   membership in the team including the fully-read ones, and Go passes all of it: a
    ///   CRT-enabled user can have unread thread *replies* in a channel whose channel-level
    ///   counters are already up to date, because a reply does not bump `TotalMsgCount`. The
    ///   thread store's own `LastReplyAt > LastViewed` clause keeps the UPDATE bounded.
    /// - **There is no `ThreadAutoFollow` gate and no `collapsedThreadsSupported`.** The thread
    ///   write happens unconditionally, and it happens *before* the early return — so a team with
    ///   nothing unread still marks its threads read.
    /// - **One team-scoped `thread_read_changed`, not one per channel**, and only when CRT is on
    ///   for the user. The client routes it to a single `ALL_TEAM_THREADS_READ` action.
    ///
    /// The channel write and its `multiple_channels_viewed` are skipped when nothing is unread,
    /// and the answer is still the full map.
    #[tracing::instrument(skip(self), fields(team_id = %team_id, user_id = %user_id, viewed))]
    pub async fn mark_team_channels_and_threads_viewed(
        &self,
        team_id: &str,
        user_id: &str,
        is_crt_enabled: bool,
    ) -> AppResult<BTreeMap<String, i64>> {
        let user = self.store().user().get(user_id).await.map_err(|err| {
            tracing::error!(error = %err, "user lookup failed");
            AppError::boxed(
                "MarkTeamChannelsAndThreadsViewed",
                "app.user.get.app_error",
                None,
                String::new(),
                500,
            )
        })?;

        let unreads = self
            .store()
            .channel()
            .get_team_channels_with_unread_and_mentions(
                team_id,
                user_id,
                user.notify_props.as_ref(),
            )
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "team unreads-and-mentions lookup failed");
                AppError::boxed(
                    "MarkTeamChannelsAndThreadsViewed",
                    "app.channel.get_channels_by_team_with_unreads_and_with_mentions.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        tracing::Span::current().record("viewed", unreads.with_unreads.len());

        self.mark_every_thread_and_the_unread_channels(
            "MarkTeamChannelsAndThreadsViewed",
            user_id,
            &unreads,
        )
        .await?;

        if is_crt_enabled {
            let mut message = mm_model::websocket_message::WebSocketEvent::new(
                mm_model::websocket_message::WEBSOCKET_EVENT_THREAD_READ_CHANGED,
                team_id,
                "",
                user_id,
                None,
                "",
            );
            message.add("timestamp", serde_json::Value::from(get_millis()));
            self.publish(message).await;
        }

        Ok(unreads.read_times)
    }

    /// Port of `App.MarkAllDirectAndGroupMessagesViewed` (channel.go:3616), behind
    /// `PUT /channels/members/{user_id}/direct/read`.
    ///
    /// Line for line its team sibling, with one difference at the end: **there is no team to
    /// broadcast on**, so the CRT event is published once per channel rather than once, and the
    /// client routes each to `ALL_THREADS_IN_CHANNEL_READ`. Every channel gets one — including
    /// the ones that were already read — because the loop is over `times`, not over the unread
    /// set, and all of them share a single timestamp.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, viewed))]
    pub async fn mark_all_direct_and_group_messages_viewed(
        &self,
        user_id: &str,
        is_crt_enabled: bool,
    ) -> AppResult<BTreeMap<String, i64>> {
        let user = self.store().user().get(user_id).await.map_err(|err| {
            tracing::error!(error = %err, "user lookup failed");
            AppError::boxed(
                "MarkAllDirectAndGroupMessagesViewed",
                "app.user.get.app_error",
                None,
                String::new(),
                500,
            )
        })?;

        let unreads = self
            .store()
            .channel()
            .get_direct_messages_with_unread_and_mentions(user_id, user.notify_props.as_ref())
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "direct unreads-and-mentions lookup failed");
                AppError::boxed(
                    "MarkAllDirectAndGroupMessagesViewed",
                    // Go reuses the *team* query's error id here (channel.go:3624); reproduced
                    // rather than corrected, since the id is what a translated message keys off.
                    "app.channel.get_channels_by_team_with_unreads_and_with_mentions.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        tracing::Span::current().record("viewed", unreads.with_unreads.len());

        self.mark_every_thread_and_the_unread_channels(
            "MarkAllDirectAndGroupMessagesViewed",
            user_id,
            &unreads,
        )
        .await?;

        if is_crt_enabled {
            let timestamp = get_millis();
            for channel_id in unreads.read_times.keys() {
                let mut message = mm_model::websocket_message::WebSocketEvent::new(
                    mm_model::websocket_message::WEBSOCKET_EVENT_THREAD_READ_CHANGED,
                    "",
                    channel_id,
                    user_id,
                    None,
                    "",
                );
                message.add("timestamp", serde_json::Value::from(timestamp));
                self.publish(message).await;
            }
        }

        Ok(unreads.read_times)
    }

    /// The body `MarkTeamChannelsAndThreadsViewed` and `MarkAllDirectAndGroupMessagesViewed`
    /// share verbatim (channel.go:3566-3600, :3627-3661).
    ///
    /// **The thread write is unconditional and comes first**; only the channel write and its
    /// event are behind "something was unread". `where_` is the caller's name, which is the only
    /// thing that differs between the two error paths.
    async fn mark_every_thread_and_the_unread_channels(
        &self,
        where_: &'static str,
        user_id: &str,
        unreads: &mm_store::UnreadsAndMentions,
    ) -> AppResult<()> {
        let all_channel_ids: Vec<String> = unreads.read_times.keys().cloned().collect();
        self.store()
            .thread()
            .mark_all_as_read_by_channels(user_id, &all_channel_ids)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "thread mark-read failed");
                AppError::boxed(
                    where_,
                    "app.thread.mark_all_as_read_by_channels.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        if unreads.with_unreads.is_empty() {
            return Ok(());
        }

        self.store()
            .channel()
            .update_last_viewed_at(&unreads.with_unreads, user_id)
            .await
            .map_err(update_last_viewed_at_error)?;

        if self.config().enable_channel_viewed_messages {
            let mut message = mm_model::websocket_message::WebSocketEvent::new(
                mm_model::websocket_message::WEBSOCKET_EVENT_MULTIPLE_CHANNELS_VIEWED,
                "",
                "",
                user_id,
                None,
                "",
            );
            message.add(
                "channel_times",
                serde_json::to_value(&unreads.read_times).unwrap_or(serde_json::Value::Null),
            );
            self.publish(message).await;
        }

        Ok(())
    }

    /// Port of `App.GetBoardChannel` (channel.go:2243), which exists only so
    /// `rejectBoardChannelByID` (api4/channel.go:23) can turn "this id is a board" into a 400 on
    /// a `/channels` route. Its **success** is the rejection; its 404 is the ordinary path.
    #[tracing::instrument(skip(self), fields(channel_id = %channel_id))]
    pub async fn get_board_channel(
        &self,
        channel_id: &str,
    ) -> AppResult<mm_model::channel::Channel> {
        self.store()
            .channel()
            .get_board_channel(channel_id)
            .await
            .map_err(|err| {
                let params = std::collections::HashMap::from([(
                    "channel_id".to_owned(),
                    serde_json::Value::String(channel_id.to_owned()),
                )]);
                if err.is_not_found() {
                    AppError::boxed(
                        "GetBoardChannel",
                        "app.channel.get.existing.app_error",
                        Some(params),
                        String::new(),
                        404,
                    )
                } else {
                    tracing::error!(error = %err, "board channel lookup failed");
                    AppError::boxed(
                        "GetBoardChannel",
                        "app.channel.get.find.app_error",
                        Some(params),
                        String::new(),
                        500,
                    )
                }
            })
    }
}

/// Port of `MarkChannelsAsViewed`'s `errors.As(err, &invErr)` split (channel.go:3707-3715).
///
/// **The two branches carry the same error id and differ only in status**, so nothing but the
/// code tells a client which happened — and the 400 is *unreachable through the route*.
/// `UpdateLastViewedAt` raises `ErrInvalidInput` only when no `Channels` row matched any id it
/// was given, and the ids it is given are `with_unreads`, which came out of a join against
/// `Channels`. Go has the same dead branch. It is reproduced rather than dropped, and this
/// function exists so that reproduction has an oracle: no request can reach it, but a unit test
/// can.
fn update_last_viewed_at_error(err: mm_store::StoreError) -> Box<AppError> {
    let status = if matches!(err, mm_store::StoreError::InvalidInput { .. }) {
        400
    } else {
        tracing::error!(error = %err, "last-viewed-at update failed");
        500
    };
    AppError::boxed(
        "MarkChannelsAsViewed",
        "app.channel.update_last_viewed_at.app_error",
        None,
        String::new(),
        status,
    )
}

#[cfg(test)]
mod tests {
    use super::update_last_viewed_at_error;

    /// The one branch no request can take, and the one it always takes.
    #[test]
    fn app_view_invalid_input_is_a_400_and_everything_else_a_500() {
        let invalid = update_last_viewed_at_error(mm_store::StoreError::InvalidInput {
            entity: "Channel",
            field: "Id",
            value: "[nosuchchannel]".to_owned(),
        });
        assert_eq!(invalid.status_code, 400);
        assert_eq!(invalid.id, "app.channel.update_last_viewed_at.app_error");

        let broken = update_last_viewed_at_error(mm_store::StoreError::NotFound {
            entity: "Channel",
            criteria: "id=x".to_owned(),
        });
        assert_eq!(broken.status_code, 500, "not-found is not invalid input");
        assert_eq!(
            broken.id, invalid.id,
            "the id is the same either way; only the status differs"
        );
    }
}
