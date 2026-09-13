//! Port of `app/post_acknowledgements.go` — `SaveAcknowledgementForPost` and
//! `DeleteAcknowledgementForPost`, the two the acknowledgement pair in `api4/post.go` reaches.
//!
//! # Where the request can still turn into a forward
//!
//! Go runs `ResolvePersistentNotification` **after** the insert and returns its error, which would
//! leave the acknowledgement written and the request failed. That ordering cannot be reproduced
//! by declining afterwards, so — exactly as `App::save_reaction_for_post` does — the decision is
//! taken before anything is written and the whole request is handed to Go. Nearly every request is
//! decided here without forwarding, because Go's own function gives up on its first lines: the
//! post's author is exempt, the feature can be off, and above all the post has to be a
//! persistent-notification post, which almost none are.
//!
//! The other forward is the post's client shape: `sendPostUpdateEvent` publishes a `post_edited`
//! carrying `PreparePostForClient(post, {IsEditPost, IncludePriority})`, and a post whose metadata
//! this process cannot build (a plugin type, a permalink embed — see
//! [`crate::post::PrepareError`]) is checked for that **before** the write, so the event is never
//! the thing that fails after the row is in.
//!
//! # Two `GetMillis()` per write
//!
//! `PreSave` stamps the acknowledgement; the store then stamps `Posts.UpdateAt` separately, so
//! the post's `update_at` is a millisecond or so *later* than `acknowledged_at`. The `post_edited`
//! event carries the post as it was read **before** the write — Go passes the pre-write `post` —
//! so its `update_at` is the old one, and the client learns the new one from its next fetch.

use mm_model::post::Post;
use mm_model::post_acknowledgement::PostAcknowledgement;
use mm_model::utils::{AppError, AppResult, get_millis};
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_ACKNOWLEDGEMENT_ADDED, WEBSOCKET_EVENT_ACKNOWLEDGEMENT_REMOVED,
    WEBSOCKET_EVENT_POST_EDITED, WebSocketEvent,
};
use mm_store::{PostAcknowledgementStore, PostStore, StoreError};

use crate::App;
use crate::post::{PrepareError, PreparePostForClientOpts};

/// `api.acknowledgement.delete.deadline.app_error` fires past this many milliseconds.
pub const UNACKNOWLEDGE_DEADLINE_MS: i64 = 5 * 60 * 1000;

/// What a write decided.
#[derive(Debug)]
pub enum AcknowledgementWrite<T> {
    Done(T),
    /// Go would do something on this request that this process cannot; the `&'static str` names
    /// the branch.
    Forward(&'static str),
}

impl App {
    /// Port of `App.SaveAcknowledgementForPost` (post_acknowledgements.go:17) via
    /// `saveAcknowledgementForPostWithPost`.
    ///
    /// In Go's order: `GetSinglePost` (its own 404), `GetChannel` (its own 404), the archived
    /// channel's **403**, the upsert, `ResolvePersistentNotification`, the two events.
    #[tracing::instrument(skip(self), fields(post_id = post_id, user_id = user_id))]
    pub async fn save_acknowledgement_for_post(
        &self,
        post_id: &str,
        user_id: &str,
    ) -> Result<AcknowledgementWrite<PostAcknowledgement>, Box<AppError>> {
        let post = self.get_single_post(post_id, false).await?;
        let channel = self.get_channel(&post.channel_id).await?;

        if channel.delete_at > 0 {
            return Err(AppError::boxed(
                "SaveAcknowledgementForPost",
                "api.acknowledgement.save.archived_channel.app_error",
                None,
                String::new(),
                403,
            ));
        }

        if let Some(why) = self.acknowledgement_undecidable(&post, user_id).await? {
            return Ok(AcknowledgementWrite::Forward(why));
        }

        // Pre-populate the ChannelId to save a DB call in store
        let acknowledgement = PostAcknowledgement {
            post_id: post.id.clone(),
            user_id: user_id.to_owned(),
            channel_id: post.channel_id.clone(),
            ..PostAcknowledgement::default()
        };

        let saved = self
            .store()
            .post_acknowledgement()
            .save_with_model(acknowledgement)
            .await
            .map_err(|err| match err {
                // `errors.As(nErr, &appErr)` — the model's own `IsValid` error passes through.
                StoreError::Invalid { app_error, .. } => app_error,
                other => {
                    tracing::error!(error = %other, "acknowledgement save failed");
                    AppError::boxed(
                        "SaveAcknowledgementForPost",
                        "app.acknowledgement.save.save.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })?;

        // `InvalidateLastPostTimeCache`: nothing here caches the last post time.
        self.send_acknowledgement_event(WEBSOCKET_EVENT_ACKNOWLEDGEMENT_ADDED, &saved, &post)
            .await;
        self.send_post_update_event(&post).await;

        Ok(AcknowledgementWrite::Done(saved))
    }

    /// Port of `App.DeleteAcknowledgementForPost` (post_acknowledgements.go:83) via
    /// `deleteAcknowledgementForPostWithPost`.
    ///
    /// The same first three steps as the save, with the delete's own archived id; then the live
    /// acknowledgement is read (**404** `app.acknowledgement.get.app_error` when there is none —
    /// under the `where` `GetPostAcknowledgement`, not the function's own name), the five-minute
    /// deadline is a **403**, and the row is zeroed.
    #[tracing::instrument(skip(self), fields(post_id = post_id, user_id = user_id))]
    pub async fn delete_acknowledgement_for_post(
        &self,
        post_id: &str,
        user_id: &str,
    ) -> Result<AcknowledgementWrite<()>, Box<AppError>> {
        let post = self.get_single_post(post_id, false).await?;
        let channel = self.get_channel(&post.channel_id).await?;

        if channel.delete_at > 0 {
            return Err(AppError::boxed(
                "DeleteAcknowledgementForPost",
                "api.acknowledgement.delete.archived_channel.app_error",
                None,
                String::new(),
                403,
            ));
        }

        // The post-edited event needs the post's client shape, as on the save path.
        if let Err(PrepareError::Unreproducible(why)) = self.prepared_for_update_event(&post).await
        {
            return Ok(AcknowledgementWrite::Forward(why));
        }

        let old = self
            .store()
            .post_acknowledgement()
            .get(&post.id, user_id)
            .await
            .map_err(|err| {
                let status = if err.is_not_found() { 404 } else { 500 };
                if status == 500 {
                    tracing::error!(error = %err, "acknowledgement lookup failed");
                }
                AppError::boxed(
                    "GetPostAcknowledgement",
                    "app.acknowledgement.get.app_error",
                    None,
                    String::new(),
                    status,
                )
            })?;

        if get_millis() - old.acknowledged_at > UNACKNOWLEDGE_DEADLINE_MS {
            return Err(AppError::boxed(
                "DeleteAcknowledgementForPost",
                "api.acknowledgement.delete.deadline.app_error",
                None,
                String::new(),
                403,
            ));
        }

        self.store()
            .post_acknowledgement()
            .delete(&old)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "acknowledgement delete failed");
                AppError::boxed(
                    "DeleteAcknowledgementForPost",
                    "app.acknowledgement.delete.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        self.send_acknowledgement_event(WEBSOCKET_EVENT_ACKNOWLEDGEMENT_REMOVED, &old, &post)
            .await;
        self.send_post_update_event(&post).await;

        Ok(AcknowledgementWrite::Done(()))
    }

    /// The two things a save can do that this process cannot, decided before the write.
    ///
    /// `ResolvePersistentNotification` (post_persistent_notification.go:20) gives up on its
    /// first lines for the author's own post, a disabled feature, or — the common case — a post
    /// that is not a persistent-notification post; only a live row past all three makes the
    /// request Go's. `post.root_id` is not consulted there, unlike the reaction path.
    async fn acknowledgement_undecidable(
        &self,
        post: &Post,
        user_id: &str,
    ) -> AppResult<Option<&'static str>> {
        if post.user_id != user_id
            && self.is_persistent_notifications_enabled()
            && self
                .store()
                .post()
                .has_persistent_notification(&post.id)
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, "persistent notification lookup failed");
                    AppError::boxed(
                        "ResolvePersistentNotification",
                        "app.post_priority.delete_persistent_notification_post.app_error",
                        None,
                        String::new(),
                        500,
                    )
                })?
        {
            return Ok(Some("the post is a persistent-notification post"));
        }
        if let Err(PrepareError::Unreproducible(why)) = self.prepared_for_update_event(post).await {
            return Ok(Some(why));
        }
        Ok(None)
    }

    /// `PreparePostForClient(post, {IsEditPost: true, IncludePriority: true})` — the shape
    /// `sendPostUpdateEvent` publishes.
    async fn prepared_for_update_event(&self, post: &Post) -> Result<Post, PrepareError> {
        self.prepare_post_for_client(
            post,
            PreparePostForClientOpts {
                is_edit_post: true,
                include_priority: true,
                ..PreparePostForClientOpts::default()
            },
        )
        .await
    }

    /// Port of `App.sendAcknowledgementEvent` (post_acknowledgements.go:239).
    ///
    /// **The acknowledgement is a JSON string inside the event's `data`, not a nested object** —
    /// `message.Add("acknowledgement", string(acknowledgementJSON))`, decoded twice by the client,
    /// the same shape as a reaction event. Channel-scoped, nobody omitted.
    async fn send_acknowledgement_event(
        &self,
        event: &str,
        acknowledgement: &PostAcknowledgement,
        post: &Post,
    ) {
        let mut message = WebSocketEvent::new(event, "", &post.channel_id, "", None, "");
        match serde_json::to_string(acknowledgement) {
            Ok(json) => message.add("acknowledgement", serde_json::Value::String(json)),
            Err(err) => {
                // Go logs and carries on, publishing an event with no `acknowledgement` key.
                tracing::warn!(error = %err, "Failed to encode acknowledgement to JSON");
            }
        }
        self.publish(message).await;
    }

    /// Port of `App.sendPostUpdateEvent` (post_acknowledgements.go:338): a `post_edited` "to
    /// trigger shared channel sync", carrying the **pre-write** post prepared for the client.
    ///
    /// The prepare was already proven possible before the write; a failure here is a store
    /// fault between the two, logged the way Go logs its own failure to publish.
    async fn send_post_update_event(&self, post: &Post) {
        match self.prepared_for_update_event(post).await {
            Ok(prepared) => {
                self.publish_websocket_event_for_post(WEBSOCKET_EVENT_POST_EDITED, &prepared)
                    .await;
            }
            Err(err) => {
                tracing::warn!(error = %err, post_id = %post.id, "Failed to send post update event for acknowledgement sync");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Five minutes, in the unit `AcknowledgedAt` is stored in. A reader who wrote `5 * 60` (seconds)
    /// would let every un-acknowledgement through; one who wrote `5 * 60 * 1000 * 1000` would let
    /// none through.
    #[test]
    fn the_deadline_is_five_minutes_of_milliseconds() {
        assert_eq!(UNACKNOWLEDGE_DEADLINE_MS, 300_000);
    }
}
