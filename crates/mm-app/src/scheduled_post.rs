//! Port of `server/channels/app/scheduled_post.go` — the four functions the api4 routes call,
//! and the event they publish.
//!
//! The scheduled-post **job** (`app/scheduled_post_job.go`) is not here: it sends due posts on a
//! timer, and no route reaches it.

use std::collections::HashMap;

use mm_model::scheduled_post::{SCHEDULED_POST_REPEAT_TYPE_NONE, ScheduledPost};
use mm_model::utils::AppError;
use mm_model::websocket_message::{
    WEBSOCKET_SCHEDULED_POST_CREATED, WEBSOCKET_SCHEDULED_POST_DELETED,
    WEBSOCKET_SCHEDULED_POST_UPDATED, WebSocketEvent,
};
use mm_store::scheduled_post_store::ScheduledPostStore;

use crate::App;
use crate::channel::RestrictedDm;
use crate::plugin_hooks::HookContext;
use crate::post::PrepareError;

/// What a scheduled-post write did, or why this server declined it.
#[derive(Debug)]
pub enum ScheduledPostWrite {
    /// Written, published, and this is the value Go answers with.
    Done(Box<ScheduledPost>),
    /// A branch this server cannot decide. The handler forwards the request whole.
    Forward(&'static str),
}

/// `recurringScheduledPostsDisabledError` (app/scheduled_post.go:22).
fn recurring_disabled(where_: &str) -> Box<AppError> {
    AppError::boxed(
        where_,
        "app.scheduled_post.recurring_disabled.app_error",
        None,
        String::new(),
        400,
    )
}

/// `map[string]any{"user_id": …, key: …}` — the params Go attaches to the app layer's errors.
/// They never reach a client whose message template does not name them, but the translation
/// renders them when it does, so they are carried.
fn params(user_id: &str, key: &str, value: &str) -> Option<HashMap<String, serde_json::Value>> {
    Some(HashMap::from([
        ("user_id".to_owned(), serde_json::json!(user_id)),
        (key.to_owned(), serde_json::json!(value)),
    ]))
}

impl App {
    /// Port of `App.SaveScheduledPost` (app/scheduled_post.go:26).
    ///
    /// # Order
    ///
    /// `PreSave`, `IsValid` against the column's size, the recurrence flag, `GetChannel`, the
    /// restricted-DM check, **then** the archived-channel check — so a restricted DM that is also
    /// archived answers the DM error — then the plugin hook, the store (which runs `PreSave`
    /// again) and the event. The hook's answer is not validated: whatever it returns is saved.
    ///
    /// # Two statuses a reader would guess wrong
    ///
    /// `IsValid`'s errors come through **unwrapped** — `model.scheduled_post.is_valid.*` or
    /// `model.draft.is_valid.*` at 400 — unlike `UpsertDraft`, which wraps its store's. And a
    /// failed insert is a **400** `app.save_scheduled_post.save.app_error`, not a 500.
    #[tracing::instrument(skip_all, fields(user_id = %scheduled_post.user_id, channel_id = %scheduled_post.channel_id))]
    pub async fn save_scheduled_post(
        &self,
        ctx: &HookContext,
        scheduled_post: ScheduledPost,
        connection_id: &str,
    ) -> Result<ScheduledPostWrite, Box<AppError>> {
        let mut scheduled_post = scheduled_post;
        let max_message_length = self.store().scheduled_post().get_max_message_size().await;
        scheduled_post.pre_save();
        scheduled_post.is_valid(max_message_length)?;

        if scheduled_post.repeat_type != SCHEDULED_POST_REPEAT_TYPE_NONE
            && !self.config().feature_flags.recurring_scheduled_posts
        {
            return Err(recurring_disabled("App.SaveScheduledPost"));
        }

        let channel = self.get_channel(&scheduled_post.channel_id).await?;

        match self.check_if_channel_is_restricted_dm(&channel).await? {
            RestrictedDm::No => {}
            RestrictedDm::Yes => {
                return Err(AppError::boxed(
                    "App.scheduledPostPreSaveChecks",
                    "app.save_scheduled_post.restricted_dm.error",
                    None,
                    String::new(),
                    400,
                ));
            }
            RestrictedDm::Undecidable => {
                return Ok(ScheduledPostWrite::Forward(
                    "a bot's exemption from DM restrictions is a plugin decision",
                ));
            }
        }

        if channel.delete_at > 0 {
            return Err(AppError::boxed(
                "App.scheduledPostPreSaveChecks",
                "app.save_scheduled_post.channel_deleted.app_error",
                params(
                    &scheduled_post.user_id,
                    "channel_id",
                    &scheduled_post.channel_id,
                ),
                String::new(),
                400,
            ));
        }

        let mut scheduled_post = self
            .run_guarded_scheduled_post_will_be_created(
                ctx,
                scheduled_post,
                "SaveScheduledPost",
                "app.scheduled_post.save.rejected_by_plugin",
            )
            .await?;

        if let Err(err) = self
            .store()
            .scheduled_post()
            .create_scheduled_post(&mut scheduled_post)
            .await
        {
            tracing::error!(error = %err, "scheduled post insert failed");
            return Err(AppError::boxed(
                "App.ScheduledPost",
                "app.save_scheduled_post.save.app_error",
                params(
                    &scheduled_post.user_id,
                    "channel_id",
                    &scheduled_post.channel_id,
                ),
                String::new(),
                400,
            ));
        }

        self.publish_scheduled_post_event(
            WEBSOCKET_SCHEDULED_POST_CREATED,
            &scheduled_post,
            connection_id,
        )
        .await;

        Ok(ScheduledPostWrite::Done(Box::new(scheduled_post)))
    }

    /// Port of `App.GetUserTeamScheduledPosts` (app/scheduled_post.go:74).
    ///
    /// A nil slice becomes an **empty** one, so the list is `[]` and never `null` — the opposite
    /// of `getDrafts`. Every post then goes through `prepareDraftWithFileInfos`, which gives it
    /// `metadata: {}` (or the files) and whose error Go ignores; a file whose mini-preview would
    /// have to be generated is the one [`PrepareError::Unreproducible`] case, and the handler
    /// forwards.
    #[tracing::instrument(skip(self), fields(found))]
    pub async fn get_user_team_scheduled_posts(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> Result<Vec<ScheduledPost>, PrepareError> {
        let mut scheduled_posts = self
            .store()
            .scheduled_post()
            .get_scheduled_posts_for_user(user_id, team_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "scheduled post lookup failed");
                PrepareError::App(AppError::boxed(
                    "App.GetUserTeamScheduledPosts",
                    "app.get_user_team_scheduled_posts.error",
                    params(user_id, "team_id", team_id),
                    String::new(),
                    500,
                ))
            })?;
        tracing::Span::current().record("found", scheduled_posts.len());

        for scheduled_post in &mut scheduled_posts {
            self.prepare_draft_with_file_infos(&mut scheduled_post.draft)
                .await?;
        }
        Ok(scheduled_posts)
    }

    /// Port of `App.UpdateScheduledPost` (app/scheduled_post.go:91).
    ///
    /// # `PreUpdate` and `IsValid` run on the client's value, before the row is read
    ///
    /// So `create_at`, `user_id` and `channel_id` must be in the **body** and valid, although
    /// `RestoreNonUpdatableFields` overwrites all three from the row a few lines later. A body
    /// without `create_at` is `model.draft.is_valid.create_at.app_error`.
    ///
    /// # Only turning recurrence *on* is refused while the flag is off
    ///
    /// Both halves must hold: the new value repeats **and** the stored one does not. Editing or
    /// ending an existing series stays possible.
    ///
    /// `error_code` and `processed_at` are reset, so an edited failed post is pending again.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, scheduled_post_id = %scheduled_post.id))]
    pub async fn update_scheduled_post(
        &self,
        ctx: &HookContext,
        user_id: &str,
        scheduled_post: ScheduledPost,
        connection_id: &str,
    ) -> Result<ScheduledPostWrite, Box<AppError>> {
        let mut scheduled_post = scheduled_post;
        let max_message_length = self.store().scheduled_post().get_max_message_size().await;
        scheduled_post.pre_update();
        scheduled_post.is_valid(max_message_length)?;

        // `Get` errors on a missing row, so Go's `existingScheduledPost == nil` 404 below it is
        // unreachable; both land here.
        let existing = self
            .store()
            .scheduled_post()
            .get(&scheduled_post.id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "scheduled post lookup failed");
                AppError::boxed(
                    "app.UpdateScheduledPost",
                    "app.update_scheduled_post.get_scheduled_post.error",
                    params(user_id, "scheduled_post_id", &scheduled_post.id),
                    String::new(),
                    500,
                )
            })?;

        if scheduled_post.repeat_type != SCHEDULED_POST_REPEAT_TYPE_NONE
            && existing.repeat_type == SCHEDULED_POST_REPEAT_TYPE_NONE
            && !self.config().feature_flags.recurring_scheduled_posts
        {
            return Err(recurring_disabled("App.UpdateScheduledPost"));
        }

        scheduled_post.restore_non_updatable_fields(&existing);
        scheduled_post.error_code = String::new();
        scheduled_post.processed_at = 0;

        let mut scheduled_post = self
            .run_guarded_scheduled_post_will_be_created(
                ctx,
                scheduled_post,
                "UpdateScheduledPost",
                "app.scheduled_post.update.rejected_by_plugin",
            )
            .await?;

        if let Err(err) = self
            .store()
            .scheduled_post()
            .updated_scheduled_post(&mut scheduled_post)
            .await
        {
            tracing::error!(error = %err, "scheduled post update failed");
            return Err(AppError::boxed(
                "app.UpdateScheduledPost",
                "app.update_scheduled_post.update.error",
                params(user_id, "scheduled_post_id", &scheduled_post.id),
                String::new(),
                500,
            ));
        }

        self.publish_scheduled_post_event(
            WEBSOCKET_SCHEDULED_POST_UPDATED,
            &scheduled_post,
            connection_id,
        )
        .await;

        Ok(ScheduledPostWrite::Done(Box::new(scheduled_post)))
    }

    /// Port of `App.DeleteScheduledPost` (app/scheduled_post.go:139).
    ///
    /// Reads the row again — the handler already has — and answers with **that** read, so the
    /// body is the stored post with no `metadata` key. The event carries the same value.
    #[tracing::instrument(skip(self, connection_id))]
    pub async fn delete_scheduled_post(
        &self,
        user_id: &str,
        scheduled_post_id: &str,
        connection_id: &str,
    ) -> Result<ScheduledPost, Box<AppError>> {
        let scheduled_post = self
            .store()
            .scheduled_post()
            .get(scheduled_post_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "scheduled post lookup failed");
                AppError::boxed(
                    "app.DeleteScheduledPost",
                    "app.delete_scheduled_post.get_scheduled_post.error",
                    params(user_id, "scheduled_post_id", scheduled_post_id),
                    String::new(),
                    500,
                )
            })?;

        if let Err(err) = self
            .store()
            .scheduled_post()
            .permanently_delete_scheduled_posts(&[scheduled_post_id.to_owned()])
            .await
        {
            tracing::error!(error = %err, "scheduled post delete failed");
            return Err(AppError::boxed(
                "app.DeleteScheduledPost",
                "app.delete_scheduled_post.delete_error",
                params(user_id, "scheduled_post_id", scheduled_post_id),
                String::new(),
                500,
            ));
        }

        self.publish_scheduled_post_event(
            WEBSOCKET_SCHEDULED_POST_DELETED,
            &scheduled_post,
            connection_id,
        )
        .await;

        Ok(scheduled_post)
    }

    /// Port of `App.PublishScheduledPostEvent` (app/scheduled_post.go:158).
    ///
    /// Addressed to the **user only** — no team, no channel, unlike the draft events, which name
    /// the channel too — and omitting the originating connection. The post travels as a JSON
    /// **string** under `scheduledPost` (camel-case, unlike every other key in the event), and
    /// it is `json.Marshal`'s string, so HTML-escaped.
    pub async fn publish_scheduled_post_event(
        &self,
        event: &str,
        scheduled_post: &ScheduledPost,
        connection_id: &str,
    ) {
        let json = match mm_model::utils::go_json_marshal(scheduled_post) {
            Ok(json) => json,
            Err(err) => {
                tracing::warn!(error = %err, "publishScheduledPostEvent - Failed to Marshal");
                return;
            }
        };
        let mut message =
            WebSocketEvent::new(event, "", "", &scheduled_post.user_id, None, connection_id);
        message.add("scheduledPost", serde_json::Value::String(json));
        self.publish(message).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_recurrence_refusal_is_a_400_with_its_own_id() {
        let err = recurring_disabled("App.SaveScheduledPost");
        assert_eq!(err.status_code, 400);
        assert_eq!(err.id, "app.scheduled_post.recurring_disabled.app_error");
        assert_eq!(err.where_, "App.SaveScheduledPost");
    }

    #[test]
    fn the_params_carry_the_user_and_the_subject() {
        let p = params("u", "channel_id", "c").unwrap();
        assert_eq!(p["user_id"], "u");
        assert_eq!(p["channel_id"], "c");
        assert_eq!(p.len(), 2);
    }
}
