//! Port of `app.GetUserStatusesByIds` (channels/app/status.go:16), which is one line over
//! `PlatformService.GetUserStatusesByIds` (channels/app/platform/status.go:136).

use mm_model::status::{STATUS_OFFLINE, Status};
use mm_model::utils::{AppError, AppResult};
use mm_store::StatusStore;

use crate::App;

/// Stand-in for `ServiceSettings.EnableUserStatuses` (config.go:712 defaults it to `true`)
/// until config is ported — the same arrangement as the privacy settings ([D-085]).
///
/// Observable when flipped: both status routes answer as if nobody had a status row — the list
/// route with `[]` and the single-user route with a 404 — rather than with "offline" for each.
pub const ENABLE_USER_STATUSES: bool = true;

impl App {
    /// Port of `PlatformService.GetUserStatusesByIds` (platform/status.go:136).
    ///
    /// # What the cache means for the port
    ///
    /// Go consults `statusCache` first and reads the database only for the misses. The cache is
    /// not ported; every id is a miss here, and `GetByIds` answers the lot. That is a faithful
    /// port of the **cold-cache** path, and the *content* matches whenever the cache and the
    /// table agree — which `SaveAndBroadcastStatus` keeps true for every status written over
    /// REST (`PUT /users/{id}/status`). Where they disagree the difference is Go's own:
    /// `SetActiveChannel` (app/channel.go:3158) and the websocket presence paths update the cache
    /// without writing the row, so a user Go has seen recently may read `online` there and
    /// `away`/`offline` here — plus a leaked `active_channel` key, since api4 writes the cached
    /// object with `json.Marshal` rather than `ToJSON`. That gap is a cache-state property, not a
    /// wire-format one; see the route notes in `MIGRATION.md`.
    ///
    /// # Order
    ///
    /// Go appends cache hits in input order, then the database rows in whatever order the query
    /// returned them, then the synthesised statuses in input order. On a warm cache — the state
    /// every request after the first sees — that is "found, in input order; then missing, in
    /// input order", and that is the order produced here: [`merge_with_offline`] sorts the rows
    /// by id (the input is already sorted, see `SortedArrayFromJSON`), so a store that returns
    /// heap order cannot leak it onto the wire.
    ///
    /// # Missing rows are not an error
    ///
    /// A user with no `Status` row — and equally an id that belongs to **no user at all** — is
    /// reported as `{user_id, status: "offline"}` with every other field zero. Go's own comment
    /// says so ("This also return the status offline for the non-existing Ids"); the single-user
    /// route therefore answers 200 for an unknown id, and its 404 branch fires only when the
    /// feature is disabled.
    #[tracing::instrument(skip_all, fields(asked = user_ids.len()))]
    pub async fn get_user_statuses_by_ids(&self, user_ids: &[String]) -> AppResult<Vec<Status>> {
        if !ENABLE_USER_STATUSES {
            return Ok(Vec::new());
        }

        let found = self
            .store()
            .status()
            .get_by_ids(user_ids)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "status lookup failed");
                AppError::boxed(
                    "GetUserStatusesByIds",
                    "app.status.get.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        Ok(merge_with_offline(user_ids, found))
    }
}

/// The tail of `GetUserStatusesByIds` (platform/status.go:185-199): the rows that came back,
/// then an offline status for every asked-for id that did not.
///
/// Go removes from `missingUserIds` each id that appears in the result, then appends a
/// `&model.Status{UserId: userID, Status: "offline"}` for what is left — `Manual` false,
/// `LastActivityAt` zero, `DNDEndTime` zero. Reproduced as a two-pass membership check rather
/// than Go's in-place splice, same outcome.
fn merge_with_offline(user_ids: &[String], mut found: Vec<Status>) -> Vec<Status> {
    // Warm-cache order: see the method doc. `sort_by` (stable) rather than `sort_unstable_by`
    // so that two rows with the same id — impossible under the primary key, but cheap to be
    // deterministic about — keep the store's relative order.
    found.sort_by(|a, b| a.user_id.cmp(&b.user_id));

    let missing: Vec<&String> = user_ids
        .iter()
        .filter(|id| !found.iter().any(|status| &status.user_id == *id))
        .collect();

    found.extend(missing.into_iter().map(|id| Status {
        user_id: id.to_owned(),
        status: STATUS_OFFLINE.to_owned(),
        ..Default::default()
    }));

    found
}

impl App {
    // ---------------------------------------------------------------------------------------
    // The status cache
    //
    // Port of `platform.statusCache`. It exists because three decisions in the setters below
    // branch on the *previous* status, and the previous status is not the `Status` row — it is
    // whatever this process last put here. A port that read the table would take a different
    // branch from Go on exactly the requests that matter, and would write rows Go throttles away.
    //
    // While the Go server is also running, the two caches are independent and can disagree about
    // whether a status changed. That is [D-182]/[D-190] again and it ends when Go does; see
    // [D-191] for why the alternative — forwarding until then — was rejected.
    // ---------------------------------------------------------------------------------------

    /// Port of `PlatformService.GetStatusFromCache` (platform/status.go).
    fn status_from_cache(&self, user_id: &str) -> Option<Status> {
        self.status_cache
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(user_id)
            .cloned()
    }

    /// Port of `PlatformService.AddStatusCache` (platform/status.go), minus the cluster send.
    fn add_status_cache(&self, status: &Status) {
        self.status_cache
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(status.user_id.clone(), status.clone());
    }

    /// Port of `PlatformService.GetStatus` (platform/status.go).
    ///
    /// **Cache first, then the table**, and the two miss differently: a cache miss falls through,
    /// while a table miss is `app.status.get.missing.app_error` at **404**. With statuses disabled
    /// it answers an *empty* `Status`, not an error — so `updateUserStatus`'s
    /// out-of-office branch sees `status.Status == ""` and does nothing.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, cached))]
    pub async fn get_status(&self, user_id: &str) -> AppResult<Status> {
        if !self.config().enable_user_statuses {
            return Ok(Status::default());
        }

        if let Some(status) = self.status_from_cache(user_id) {
            tracing::Span::current().record("cached", true);
            return Ok(status);
        }
        tracing::Span::current().record("cached", false);

        match self.store().status().get(user_id).await {
            Ok(Some(status)) => Ok(status),
            Ok(None) => Err(AppError::boxed(
                "GetStatus",
                "app.status.get.missing.app_error",
                None,
                String::new(),
                404,
            )),
            Err(err) => {
                tracing::error!(error = %err, "status lookup failed");
                Err(AppError::boxed(
                    "GetStatus",
                    "app.status.get.app_error",
                    None,
                    String::new(),
                    500,
                ))
            }
        }
    }

    /// Port of `PlatformService.SaveAndBroadcastStatus` (platform/status.go).
    ///
    /// Cache, then table, then socket — and **a failed write is logged, not returned**, so the
    /// cache and the broadcast go ahead regardless. A client is told the status changed even when
    /// the row did not.
    async fn save_and_broadcast_status(&self, status: &Status) {
        self.add_status_cache(status);

        if let Err(err) = self.store().status().save_or_update(status).await {
            tracing::warn!(error = %err, user_id = %status.user_id, "Failed to save status");
        }

        self.broadcast_status(status).await;
    }

    /// Port of `PlatformService.BroadcastStatus` (platform/status.go).
    ///
    /// Addressed to the **user**, and carrying only `status` and `user_id` — not the `Status`
    /// object. Go skips it entirely when the server is busy, which this port has no equivalent of
    /// and which only ever *suppresses* an event.
    async fn broadcast_status(&self, status: &Status) {
        let mut event = mm_model::websocket_message::WebSocketEvent::new(
            mm_model::websocket_message::WEBSOCKET_EVENT_STATUS_CHANGE,
            "",
            "",
            &status.user_id,
            None,
            "",
        );
        event.add("status", serde_json::Value::String(status.status.clone()));
        event.add("user_id", serde_json::Value::String(status.user_id.clone()));
        self.publish(event).await;
    }

    /// Port of `PlatformService.SetStatusOnline` (platform/status.go).
    ///
    /// # The one setter that can decide *not* to write the row
    ///
    /// Three things follow from the previous status, and all three are why the cache exists:
    ///
    /// - `status.Manual && !manual` returns early — a manually set status overrides a non-manual
    ///   one, so a background heartbeat cannot clear a user's explicit "away";
    /// - `broadcast` is set only when the status was **not already online** (or there was no
    ///   status at all), so a redundant online publishes nothing;
    /// - the row is written only when the status changed, the manual flag changed, **or**
    ///   `LastActivityAt` moved by more than `StatusMinUpdateTime`. Inside that window the
    ///   activity is kept in the cache and never reaches the table.
    ///
    /// And when it does write, *which* statement depends on `broadcast`: `SaveOrUpdate` for a
    /// real change, `UpdateLastActivityAt` for a heartbeat. Only the first is ported — see the
    /// note in the body.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, manual, broadcast))]
    pub async fn set_status_online(&self, user_id: &str, manual: bool) {
        if !self.config().enable_user_statuses {
            return;
        }

        let mut broadcast = false;
        let mut old_status = STATUS_OFFLINE.to_owned();
        let mut old_time = 0i64;
        let mut old_manual = false;

        let mut status = match self.get_status(user_id).await {
            Ok(status) if !status.user_id.is_empty() => {
                if status.manual && !manual {
                    return; // manually set status always overrides non-manual one
                }
                if status.status != mm_model::status::STATUS_ONLINE {
                    broadcast = true;
                }
                old_status = status.status.clone();
                old_time = status.last_activity_at;
                old_manual = status.manual;

                let mut status = status;
                status.status = mm_model::status::STATUS_ONLINE.to_owned();
                // "for online there's no manual setting" — the flag is cleared even when the
                // caller asked for a manual online.
                status.manual = false;
                status.last_activity_at = mm_model::utils::get_millis();
                status
            }
            _ => {
                broadcast = true;
                Status {
                    user_id: user_id.to_owned(),
                    status: mm_model::status::STATUS_ONLINE.to_owned(),
                    manual: false,
                    last_activity_at: mm_model::utils::get_millis(),
                    active_channel: String::new(),
                    ..Status::default()
                }
            }
        };
        tracing::Span::current().record("broadcast", broadcast);

        self.add_status_cache(&status);

        if online_row_needs_writing(&status, &old_status, old_manual, old_time) {
            if broadcast {
                if let Err(err) = self.store().status().save_or_update(&status).await {
                    tracing::warn!(error = %err, user_id, "Failed to save status");
                }
            } else {
                // Go calls `UpdateLastActivityAt` here, which writes **only** that column.
                // `SaveOrUpdate` writes the whole row — and every other column on `status` came
                // from the row this function just read, so the result is the same values. Using
                // one statement rather than two is a divergence in *how*, not in *what*.
                if let Err(err) = self.store().status().save_or_update(&status).await {
                    tracing::error!(error = %err, user_id, "Failed to save status");
                }
            }
        }

        if broadcast {
            // Re-read the cached copy: `SaveAndBroadcastStatus` is not used here, so the
            // broadcast is explicit.
            status.active_channel = String::new();
            self.broadcast_status(&status).await;
        }
    }

    /// Port of `PlatformService.SetStatusOffline` (platform/status.go).
    ///
    /// The manual-override guard has a `force` escape that no REST caller sets, and the status it
    /// writes is **built fresh** rather than patched — so `PrevStatus` and `DNDEndTime` are
    /// cleared by going offline.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, manual))]
    pub async fn set_status_offline(&self, user_id: &str, manual: bool, force: bool) {
        if !self.config().enable_user_statuses {
            return;
        }

        match self.get_status(user_id).await {
            // manually set status always overrides non-manual one
            Ok(status) if !status.user_id.is_empty() && !force && status.manual && !manual => {
                return;
            }
            // Go logs and carries on: "Error getting status. Setting it to offline forcefully."
            _ => {}
        }

        let status = Status {
            user_id: user_id.to_owned(),
            status: STATUS_OFFLINE.to_owned(),
            manual,
            last_activity_at: mm_model::utils::get_millis(),
            active_channel: String::new(),
            ..Status::default()
        };
        self.save_and_broadcast_status(&status).await;
    }

    /// Port of `PlatformService.SetStatusAwayIfNeeded` (platform/status.go).
    ///
    /// **"IfNeeded" is three conditions, and all of them are skipped for a manual request.** A
    /// non-manual away is dropped when the user is already away, and dropped again when they have
    /// been active more recently than `UserStatusAwayTimeout`. A manual away is unconditional.
    ///
    /// A missing status is *not* an error here: Go substitutes an offline placeholder whose
    /// `Manual` is the caller's own flag — so `!manual && status.Manual` is false for a
    /// non-manual request on a user with no row, and the activity test then decides. With
    /// `LastActivityAt` zero, `isUserAway` is true, so the away lands.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, manual))]
    pub async fn set_status_away_if_needed(&self, user_id: &str, manual: bool) {
        if !self.config().enable_user_statuses {
            return;
        }

        let mut status = match self.get_status(user_id).await {
            Ok(status) if !status.user_id.is_empty() => status,
            _ => Status {
                user_id: user_id.to_owned(),
                status: STATUS_OFFLINE.to_owned(),
                manual,
                last_activity_at: 0,
                active_channel: String::new(),
                ..Status::default()
            },
        };

        if !away_is_needed(
            &status,
            manual,
            mm_model::utils::get_millis(),
            self.config().user_status_away_timeout,
        ) {
            return;
        }

        status.status = mm_model::status::STATUS_AWAY.to_owned();
        status.manual = manual;
        status.active_channel = String::new();

        self.save_and_broadcast_status(&status).await;
    }

    /// Port of `PlatformService.SetStatusDoNotDisturbTimed` (platform/status.go).
    ///
    /// **No guard at all** — not even the manual override every other setter carries — and it
    /// records `PrevStatus` so the expiry job can restore it. `Manual` is forced true.
    ///
    /// `DNDEndTime` is **seconds**, unlike every other timestamp in the package, and is truncated
    /// down to a whole `DNDExpiryInterval` (a minute).
    #[tracing::instrument(skip(self), fields(user_id = %user_id, end_time))]
    pub async fn set_status_do_not_disturb_timed(&self, user_id: &str, end_time: i64) {
        if !self.config().enable_user_statuses {
            return;
        }

        let mut status = match self.get_status(user_id).await {
            Ok(status) if !status.user_id.is_empty() => status,
            _ => Status {
                user_id: user_id.to_owned(),
                status: STATUS_OFFLINE.to_owned(),
                manual: false,
                last_activity_at: 0,
                active_channel: String::new(),
                ..Status::default()
            },
        };

        status.prev_status = status.status.clone();
        status.status = mm_model::status::STATUS_DND.to_owned();
        status.manual = true;
        status.dnd_end_time = truncate_dnd_end_time(end_time);

        self.save_and_broadcast_status(&status).await;
    }
}

/// Port of `PlatformService.isUserAway` (platform/status.go).
///
/// `LastActivityAt` is **milliseconds** and `UserStatusAwayTimeout` is **seconds**, hence the
/// `* 1000`. The comparison is `>=`, so a user idle for exactly the timeout is away.
fn is_user_away(now: i64, last_activity_at: i64, away_timeout_secs: i64) -> bool {
    now - last_activity_at >= away_timeout_secs * 1000
}

/// The three guards at the top of `SetStatusAwayIfNeeded` (platform/status.go), as one decision.
///
/// **"IfNeeded" is three conditions, and all of them are skipped for a manual request.** A
/// non-manual away is dropped when a manual status is already in place, dropped again when the
/// user is already away, and dropped a third time when they have been active more recently than
/// `UserStatusAwayTimeout`. A manual away is unconditional.
///
/// Extracted because the only route that reaches this function passes `manual: true`, so every
/// `false` branch is unreachable over HTTP and the parity suite cannot see it. The decision is
/// pure, so it is tested directly instead — a port whose guards were reachable-but-wrong would
/// otherwise ship unexamined.
fn away_is_needed(status: &Status, manual: bool, now: i64, away_timeout_secs: i64) -> bool {
    if manual {
        return true;
    }
    // manually set status always overrides non-manual one
    if status.manual {
        return false;
    }
    if status.status == mm_model::status::STATUS_AWAY {
        return false;
    }
    is_user_away(now, status.last_activity_at, away_timeout_secs)
}

/// The write test in the middle of `SetStatusOnline` (platform/status.go).
///
/// The row is written only when the status changed, the manual flag changed, **or**
/// `LastActivityAt` moved by more than `StatusMinUpdateTime`. Inside that window a heartbeat is
/// kept in the cache and never reaches the table — which is the whole reason the cache exists,
/// and which no single HTTP request can demonstrate.
///
/// The comparison is strictly `>`: a move of exactly `StatusMinUpdateTime` does not write.
fn online_row_needs_writing(
    new: &Status,
    old_status: &str,
    old_manual: bool,
    old_time: i64,
) -> bool {
    new.status != old_status
        || new.manual != old_manual
        || new.last_activity_at - old_time > mm_model::status::STATUS_MIN_UPDATE_TIME
}

impl App {
    // ---------------------------------------------------------------------------------------
    // Custom statuses
    //
    // A custom status is **not** a `Status` row. It lives in `Users.Props["customStatus"]` as a
    // JSON string, so every write here is a full `UpdateUser` — which is why these four routes
    // were blocked on that function rather than on the status cache.
    // ---------------------------------------------------------------------------------------

    /// Port of `App.confirmEmojiExists` (app/emoji.go:381).
    ///
    /// A system emoji short-circuits; anything else must pass `IsValidEmojiName` **and** exist in
    /// the `Emoji` table. The two failures have different ids, and both are the emoji route's own
    /// rather than a custom-status one.
    async fn confirm_emoji_exists(&self, emoji_name: &str) -> AppResult<()> {
        if mm_model::emoji::is_system_emoji_name(emoji_name) {
            return Ok(());
        }
        mm_model::emoji::is_valid_emoji_name(emoji_name)?;
        self.get_emoji_by_name(emoji_name).await?;
        Ok(())
    }

    /// Port of `App.SetCustomStatus` (app/status.go:84).
    ///
    /// # The emoji is checked before anything is written
    ///
    /// "Ensure the emoji exists before saving the custom status even if it's deleted afterwards"
    /// — and the wrapper error is `api.custom_status.set_custom_statuses.emoji_not_found` at 400,
    /// which **replaces** the emoji route's own 404. A port that let `confirmEmojiExists`'s error
    /// through would answer 404 where Go answers 400.
    ///
    /// # Two failures that are logged, not returned
    ///
    /// `user.SetCustomStatus` failing (a marshalling error, unreachable for this type) and
    /// `addRecentCustomStatus` failing are both logged and swallowed. So a client can get a 200
    /// for a status that was stored but whose recents were not updated.
    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    pub async fn set_custom_status(
        &self,
        user_id: &str,
        cs: &mm_model::custom_status::CustomStatus,
    ) -> AppResult<()> {
        if cs.emoji.is_empty() && cs.text.is_empty() {
            return Err(AppError::boxed(
                "SetCustomStatus",
                "api.custom_status.set_custom_statuses.update.app_error",
                None,
                String::new(),
                400,
            ));
        }

        if !cs.emoji.is_empty() && self.confirm_emoji_exists(&cs.emoji).await.is_err() {
            return Err(AppError::boxed(
                "SetCustomStatus",
                "api.custom_status.set_custom_statuses.emoji_not_found",
                None,
                String::new(),
                400,
            ));
        }

        let mut user = self.get_user(user_id).await?;
        if let Err(err) = user.set_custom_status(cs) {
            tracing::error!(error = %err, user_id, "Failed to set custom status");
        }
        self.update_user(&user, true).await?;

        if let Err(err) = self.add_recent_custom_status(user_id, cs).await {
            tracing::error!(error = %err, user_id, "Can't add recent custom status for");
        }

        Ok(())
    }

    /// Port of `App.RemoveCustomStatus` (app/status.go:116).
    ///
    /// `ClearCustomStatus` writes the **empty string** into the prop rather than removing the
    /// key, so a cleared status and a never-set one are different rows. It does not touch the
    /// recents.
    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    pub async fn remove_custom_status(&self, user_id: &str) -> AppResult<()> {
        let mut user = self.get_user(user_id).await?;
        user.clear_custom_status();
        self.update_user(&user, true).await?;
        Ok(())
    }

    /// Port of `App.addRecentCustomStatus` (app/status.go:140).
    ///
    /// **An error from the preference read is treated the same as an empty value**: the recents
    /// become a one-element list. So a user with no preference row and a user whose store is
    /// failing both end up with their history replaced rather than extended.
    async fn add_recent_custom_status(
        &self,
        user_id: &str,
        status: &mm_model::custom_status::CustomStatus,
    ) -> AppResult<()> {
        let existing = self
            .get_preference_by_category_and_name_for_user(
                user_id,
                mm_model::preference::PREFERENCE_CATEGORY_CUSTOM_STATUS,
                mm_model::preference::PREFERENCE_NAME_RECENT_CUSTOM_STATUSES,
            )
            .await
            .ok()
            .filter(|preference| !preference.value.is_empty());

        let new_rcs = match existing {
            None => mm_model::custom_status::RecentCustomStatuses(vec![status.clone()]),
            Some(preference) => {
                let decoded: mm_model::custom_status::RecentCustomStatuses =
                    serde_json::from_str(&preference.value).map_err(|err| {
                        tracing::debug!(error = %err, "recent custom statuses did not decode");
                        AppError::boxed(
                            "addRecentCustomStatus",
                            "api.unmarshal_error",
                            None,
                            String::new(),
                            400,
                        )
                    })?;
                decoded.add(status)
            }
        };

        let encoded = serde_json::to_string(&new_rcs).map_err(|err| {
            tracing::error!(error = %err, "recent custom statuses did not encode");
            AppError::boxed(
                "addRecentCustomStatus",
                "api.marshal_error",
                None,
                String::new(),
                400,
            )
        })?;

        self.update_preferences(
            user_id,
            &mm_model::preference::Preferences(vec![mm_model::preference::Preference {
                user_id: user_id.to_owned(),
                category: mm_model::preference::PREFERENCE_CATEGORY_CUSTOM_STATUS.to_owned(),
                name: mm_model::preference::PREFERENCE_NAME_RECENT_CUSTOM_STATUSES.to_owned(),
                value: encoded,
            }]),
        )
        .await
    }

    /// Port of `App.RemoveRecentCustomStatus` (app/status.go:171).
    ///
    /// **Four different ways to answer the same 400.** A failed read is the preference route's own
    /// error; an empty value, a status the list does not contain, and a marshalling failure are
    /// all `api.custom_status.recent_custom_statuses.delete.app_error`. The membership test is a
    /// **byte comparison of the marshalled status**, not a field match — so a request whose
    /// `expires_at` differs by a second removes nothing and is a 400.
    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    pub async fn remove_recent_custom_status(
        &self,
        user_id: &str,
        status: &mm_model::custom_status::CustomStatus,
    ) -> AppResult<()> {
        let preference = self
            .get_preference_by_category_and_name_for_user(
                user_id,
                mm_model::preference::PREFERENCE_CATEGORY_CUSTOM_STATUS,
                mm_model::preference::PREFERENCE_NAME_RECENT_CUSTOM_STATUSES,
            )
            .await?;

        if preference.value.is_empty() {
            return Err(recent_delete_error());
        }

        let existing: mm_model::custom_status::RecentCustomStatuses =
            serde_json::from_str(&preference.value).map_err(|err| {
                tracing::debug!(error = %err, "recent custom statuses did not decode");
                AppError::boxed(
                    "RemoveRecentCustomStatus",
                    "api.unmarshal_error",
                    None,
                    String::new(),
                    400,
                )
            })?;

        match existing.contains(status) {
            Ok(true) => {}
            Ok(false) | Err(_) => return Err(recent_delete_error()),
        }

        let new_rcs = existing.remove(status).map_err(|_| recent_delete_error())?;
        let encoded = serde_json::to_string(&new_rcs).map_err(|err| {
            tracing::error!(error = %err, "recent custom statuses did not encode");
            AppError::boxed(
                "RemoveRecentCustomStatus",
                "api.marshal_error",
                None,
                String::new(),
                400,
            )
        })?;

        // Go mutates the preference it read and writes *that* back, so the row keeps whatever
        // `UserId`/`Category`/`Name` the store returned rather than the arguments.
        let mut preference = preference;
        preference.value = encoded;
        self.update_preferences(
            user_id,
            &mm_model::preference::Preferences(vec![preference]),
        )
        .await
    }
}

/// The one error id `RemoveRecentCustomStatus` gives three different failures.
fn recent_delete_error() -> Box<AppError> {
    AppError::boxed(
        "RemoveRecentCustomStatus",
        "api.custom_status.recent_custom_statuses.delete.app_error",
        None,
        String::new(),
        400,
    )
}

/// Port of `truncateDNDEndTime` (platform/status.go).
///
/// `time.Unix(endtime, 0).Truncate(DNDExpiryInterval).Unix()` — a **seconds** value rounded down
/// to a whole minute. `Truncate` floors toward the epoch, and Go's `time.Truncate` operates on the
/// duration since the zero time rather than since the epoch — but for a positive Unix second the
/// two agree, and a negative end time is not reachable through the route.
fn truncate_dnd_end_time(end_time: i64) -> i64 {
    let interval = mm_model::status::DND_EXPIRY_INTERVAL.as_secs() as i64;
    if interval <= 0 {
        return end_time;
    }
    end_time - end_time.rem_euclid(interval)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(user_id: &str, state: &str) -> Status {
        Status {
            user_id: user_id.to_owned(),
            status: state.to_owned(),
            manual: true,
            last_activity_at: 1_701_355_039_000,
            dnd_end_time: 58,
            ..Default::default()
        }
    }

    /// The synthesised status is exactly `{UserId, Status: "offline"}` — every other field at
    /// its zero value, not copied from anywhere.
    #[test]
    fn a_missing_row_becomes_a_zeroed_offline_status() {
        let ids = vec!["aaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned()];
        let merged = merge_with_offline(&ids, Vec::new());

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].user_id, "aaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(merged[0].status, "offline");
        assert!(!merged[0].manual);
        assert_eq!(merged[0].last_activity_at, 0);
        assert_eq!(merged[0].dnd_end_time, 0);
        assert_eq!(merged[0].active_channel, "");
        assert_eq!(merged[0].prev_status, "");
    }

    /// Found first in id order, then the missing in input order — and a found id is never
    /// *also* synthesised.
    #[test]
    fn found_rows_come_first_then_the_missing_in_input_order() {
        let ids: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.repeat(26)).collect();
        // The store's order is deliberately not the input's.
        let found = vec![
            status(&"d".repeat(26), "away"),
            status(&"b".repeat(26), "dnd"),
        ];

        let merged = merge_with_offline(&ids, found);
        let order: Vec<(&str, &str)> = merged
            .iter()
            .map(|s| (&s.user_id[..1], s.status.as_str()))
            .collect();

        assert_eq!(
            order,
            vec![
                ("b", "dnd"),
                ("d", "away"),
                ("a", "offline"),
                ("c", "offline")
            ]
        );
    }

    /// A found row keeps every field the store gave it — the merge adds, it does not rewrite.
    #[test]
    fn a_found_row_is_passed_through_untouched() {
        let ids = vec!["b".repeat(26)];
        let merged = merge_with_offline(&ids, vec![status(&"b".repeat(26), "dnd")]);
        assert_eq!(merged, vec![status(&"b".repeat(26), "dnd")]);
    }

    /// No ids in, nothing out — the single-user route's 404 branch depends on an empty list
    /// meaning "nothing", never on an error.
    #[test]
    fn no_ids_yields_an_empty_list() {
        assert!(merge_with_offline(&[], Vec::new()).is_empty());
    }

    /// `truncateDNDEndTime` floors a **seconds** value to a whole minute. A value already on the
    /// boundary is unchanged; 59 seconds past it goes back to the boundary, not forward.
    #[test]
    fn the_dnd_end_time_is_floored_to_a_whole_minute() {
        assert_eq!(truncate_dnd_end_time(2_000_000_040), 2_000_000_040);
        assert_eq!(truncate_dnd_end_time(2_000_000_045), 2_000_000_040);
        assert_eq!(truncate_dnd_end_time(2_000_000_099), 2_000_000_040);
        assert_eq!(truncate_dnd_end_time(0), 0);
        assert_eq!(truncate_dnd_end_time(59), 0);
        assert_eq!(truncate_dnd_end_time(60), 60);
    }

    /// `rem_euclid`, not `%`: a negative end time is not reachable through the route, but the
    /// two operators disagree there and `time.Truncate` floors, so the Euclidean form is the one
    /// that matches. -1 second is one second *before* the epoch, which floors to -60.
    #[test]
    fn a_negative_end_time_floors_away_from_zero() {
        assert_eq!(truncate_dnd_end_time(-1), -60);
        assert_eq!(truncate_dnd_end_time(-60), -60);
        assert_eq!(truncate_dnd_end_time(-61), -120);
    }

    fn plain(state: &str, manual: bool, last_activity_at: i64) -> Status {
        Status {
            user_id: "u".repeat(26),
            status: state.to_owned(),
            manual,
            last_activity_at,
            ..Default::default()
        }
    }

    /// A **manual** away skips all three guards — this is the only path the REST route takes, so
    /// a port that lost the short circuit would still pass every parity test.
    #[test]
    fn a_manual_away_is_unconditional() {
        let now = 1_700_000_000_000;
        for status in ["away", "online", "dnd", "offline"] {
            for was_manual in [true, false] {
                assert!(
                    away_is_needed(&plain(status, was_manual, now), true, now, 300),
                    "a manual away was dropped over {status}/manual={was_manual}"
                );
            }
        }
    }

    /// A non-manual away loses to a manual status, whatever that status is.
    #[test]
    fn a_non_manual_away_loses_to_a_manual_status() {
        let now = 1_700_000_000_000;
        for status in ["online", "dnd", "offline"] {
            assert!(
                !away_is_needed(&plain(status, true, 0), false, now, 300),
                "a non-manual away displaced a manual {status}"
            );
            // …and the same status non-manually held, with the activity long enough ago, does
            // let it through — so the guard above is the manual flag and not the status.
            assert!(away_is_needed(&plain(status, false, 0), false, now, 300));
        }
    }

    /// Already away, non-manually: nothing to do.
    #[test]
    fn a_non_manual_away_over_away_is_dropped() {
        let now = 1_700_000_000_000;
        assert!(!away_is_needed(&plain("away", false, 0), false, now, 300));
        // The same state asked for manually still lands.
        assert!(away_is_needed(&plain("away", false, 0), true, now, 300));
    }

    /// The activity test is the last guard, and it is `>=` on a **seconds** timeout against a
    /// **milliseconds** field. Exactly the timeout is away; one millisecond less is not.
    #[test]
    fn the_activity_window_is_inclusive_and_in_seconds() {
        let now = 1_700_000_000_000;
        let timeout = 300;
        assert!(!away_is_needed(
            &plain("online", false, now - 299_999),
            false,
            now,
            timeout
        ));
        assert!(away_is_needed(
            &plain("online", false, now - 300_000),
            false,
            now,
            timeout
        ));
        assert!(is_user_away(now, now - 300_000, timeout));
        assert!(!is_user_away(now, now - 299_999, timeout));
        // A user with no row has `LastActivityAt` zero, which is always away.
        assert!(is_user_away(now, 0, timeout));
    }

    /// The online write test: any of three reasons, and the activity one is strictly `>`.
    #[test]
    fn the_online_row_is_written_for_three_reasons_only() {
        let now = 1_700_000_000_000;
        let fresh = plain("online", false, now);

        // Nothing moved.
        assert!(!online_row_needs_writing(&fresh, "online", false, now));
        // The status changed.
        assert!(online_row_needs_writing(&fresh, "away", false, now));
        // The manual flag changed — `SetStatusOnline` clears it, so this fires when the previous
        // status was a manual one.
        assert!(online_row_needs_writing(&fresh, "online", true, now));
        // The activity moved by more than the minimum. Exactly the minimum does not write.
        let min = mm_model::status::STATUS_MIN_UPDATE_TIME;
        assert!(!online_row_needs_writing(
            &fresh,
            "online",
            false,
            now - min
        ));
        assert!(online_row_needs_writing(
            &fresh,
            "online",
            false,
            now - min - 1
        ));
    }
}
