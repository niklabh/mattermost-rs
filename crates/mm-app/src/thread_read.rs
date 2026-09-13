//! Port of the per-thread read-state writes in `server/channels/app/user.go` —
//! `UpdateThreadReadForUser` (:3234) and `UpdateThreadReadForUserByPost` (:3221) — and the
//! mention count they write, `countThreadMentions` (app/post.go:2505).
//!
//! Behind `PUT /users/{user_id}/teams/{team_id}/threads/{thread_id}/read/{timestamp}` and
//! `POST …/threads/{thread_id}/set_unread/{post_id}`. Kept out of [`crate::thread`], which
//! holds the reads and the family-wide writes.
//!
//! # `UnreadMentions` is written here and read by the Go server on every threads-list request
//!
//! That is what made this route wait for the mention engine ([D-250], now closed): a count that
//! was merely close would sit in a shared column with nothing to correct it. The count is
//! [`App::count_thread_mentions`], which is Go's function branch for branch, on top of
//! [`crate::mention`] and the markdown walker.
//!
//! # Five statements, in Go's order, and the order is observable
//!
//! 1. the previous unread-reply count is read **before** anything moves (it goes on the event);
//! 2. `UpdateMembership` writes the new mention count with the row's **old** `LastViewed` and
//!    `LastUpdated`;
//! 3. `MarkAsRead` then moves `LastViewed` to the timestamp and `LastUpdated` to now;
//! 4. `GetThreadForUser` reads the thread back with the membership's `LastViewed` **already set
//!    to the timestamp in memory** — so `unread_replies` on the wire is counted from the new
//!    mark, while `last_viewed_at` is that same timestamp;
//! 5. the event goes out with both the new and the previous counters.
//!
//! Swapping 2 and 3 would leave `LastUpdated` at the clock either way; swapping 3 and 4 would
//! report the old unread count. Both were mutated and both were caught.

use mm_model::channel::{CHANNEL_TYPE_DIRECT, CHANNEL_TYPE_GROUP};
use mm_model::post::Post;
use mm_model::status::{STATUS_ONLINE, Status};
use mm_model::thread::ThreadResponse;
use mm_model::user::User;
use mm_model::utils::{AppError, AppResult, StringMap};
use mm_model::websocket_message::{WEBSOCKET_EVENT_THREAD_READ_CHANGED, WebSocketEvent};
use mm_store::post_store::PostStore;
use mm_store::thread_store::ThreadStore;

use crate::App;
use crate::mention::{MentionKeywords, get_explicit_mentions};

/// `FeatureFlags.MmBlocksEnabled` (feature_flags.go:138), defaulted **`true`** at :214 and
/// `true` on this deployment — the same literal `crate::post::get_emoji_names_for_post` and
/// `crate::post_write` carry, for the same reason.
const MM_BLOCKS_ENABLED: bool = true;

impl App {
    /// Port of `app.App.UpdateThreadReadForUserByPost` (app/user.go:3221).
    ///
    /// The post must be **in** the thread — its `RootId` is the thread, or it *is* the thread —
    /// or the answer is a 400 whose `where` is `UpdateThreadReadForUser`, the callee's name
    /// rather than this function's. The mark then moves to one millisecond **before** the post,
    /// which is what makes that post unread under the store's strict `>`.
    #[tracing::instrument(
        skip(self),
        fields(user_id = %user_id, team_id = %team_id, thread_id = %thread_id, post_id = %post_id)
    )]
    pub async fn update_thread_read_for_user_by_post(
        &self,
        current_session_id: &str,
        user_id: &str,
        team_id: &str,
        thread_id: &str,
        post_id: &str,
    ) -> AppResult<ThreadResponse> {
        let post = self.get_single_post(post_id, false).await?;

        if post.root_id != thread_id && post_id != thread_id {
            return Err(AppError::boxed(
                "UpdateThreadReadForUser",
                "app.user.update_thread_read_for_user_by_post.app_error",
                None,
                String::new(),
                400,
            ));
        }

        self.update_thread_read_for_user(
            current_session_id,
            user_id,
            team_id,
            thread_id,
            post.create_at - 1,
        )
        .await
    }

    /// Port of `app.App.UpdateThreadReadForUser` (app/user.go:3234). See the module docs for
    /// the statement order.
    ///
    /// # Two lookups before any write, and each refuses differently
    ///
    /// `GetUser` first — a deleted or unknown user is `app.user.missing_account.const` — then
    /// the membership, whose absence is `app.user.get_thread_membership_for_user.not_found`.
    /// "If the thread doesn't have a membership, we shouldn't try to mark it as unread": a
    /// caller who never followed the thread gets a 404, not a row.
    ///
    /// # `clearPushNotification` is not called
    ///
    /// Go clears the mobile badge when the thread is fully read and collapsed threads are on
    /// for the user. This server has no push hub ([D-215]); the condition is evaluated and
    /// traced so the decision is visible, and the call it guards does not exist yet.
    #[tracing::instrument(
        skip(self),
        fields(user_id = %user_id, team_id = %team_id, thread_id = %thread_id, timestamp, unread_mentions, unread_replies)
    )]
    pub async fn update_thread_read_for_user(
        &self,
        current_session_id: &str,
        user_id: &str,
        team_id: &str,
        thread_id: &str,
        timestamp: i64,
    ) -> AppResult<ThreadResponse> {
        let wrap = |err: mm_store::error::StoreError| {
            tracing::error!(error = %err, "thread read update failed");
            AppError::boxed(
                "UpdateThreadReadForUser",
                "app.user.update_thread_read_for_user.app_error",
                None,
                String::new(),
                500,
            )
        };

        let user = self.get_user(user_id).await?;

        // If the thread doesn't have a membership, we shouldn't try to mark it as unread.
        let mut membership = self
            .get_thread_membership_for_user(user_id, thread_id)
            .await?;

        let previous_unread_mentions = membership.unread_mentions;
        let previous_unread_replies = self
            .store()
            .thread()
            .get_thread_unread_reply_count(&membership)
            .await
            .map_err(wrap)?;

        let post = self.get_single_post(thread_id, false).await?;
        membership.unread_mentions = self
            .count_thread_mentions(&user, &post, team_id, timestamp)
            .await?;
        tracing::Span::current().record("unread_mentions", membership.unread_mentions);

        self.store()
            .thread()
            .update_membership(&membership)
            .await
            .map_err(wrap)?;

        membership.last_viewed = timestamp;

        self.store()
            .thread()
            .mark_as_read(user_id, thread_id, timestamp)
            .await
            .map_err(wrap)?;

        let thread = self.get_thread_for_user(&membership, false).await?;
        tracing::Span::current().record("unread_replies", thread.unread_replies);

        // Clear if user has read the messages.
        if thread.unread_replies == 0 && self.is_crt_enabled_for_user(user_id).await {
            // `a.clearPushNotification(currentSessionId, userID, post.ChannelId, threadID)` —
            // no push hub, see [D-215].
            tracing::debug!(
                session_id = %current_session_id,
                channel_id = %post.channel_id,
                "thread fully read; the push-notification clear has no hub to reach"
            );
        }

        let mut message = WebSocketEvent::new(
            WEBSOCKET_EVENT_THREAD_READ_CHANGED,
            team_id,
            "",
            user_id,
            None,
            "",
        );
        message.add("thread_id", serde_json::Value::String(thread_id.to_owned()));
        message.add("timestamp", serde_json::Value::from(timestamp));
        message.add(
            "unread_mentions",
            serde_json::Value::from(membership.unread_mentions),
        );
        message.add(
            "unread_replies",
            serde_json::Value::from(thread.unread_replies),
        );
        message.add(
            "previous_unread_mentions",
            serde_json::Value::from(previous_unread_mentions),
        );
        message.add(
            "previous_unread_replies",
            serde_json::Value::from(previous_unread_replies),
        );
        message.add(
            "channel_id",
            serde_json::Value::String(post.channel_id.clone()),
        );
        self.publish(message).await;

        Ok(thread)
    }

    /// Port of `app.App.countThreadMentions` (app/post.go:2505): how many replies in `post`'s
    /// thread, created at or after `timestamp`, mention `user`.
    ///
    /// # In a DM or group message every post by the other side is a mention
    ///
    /// No keywords at all: the count is the replies whose author is
    /// [`mm_model::channel::Channel::get_other_user_id_for_dm`]. For a **group** message that
    /// helper answers `""` — its name is not `id__id` — so nothing matches and a GM thread
    /// counts zero mentions however many replies it has. That is Go's answer too, and it is
    /// reproduced rather than repaired.
    ///
    /// # Everywhere else the keywords are the user's, plus the groups the channel may reference
    ///
    /// `MentionKeywords.AddUser` with an **empty** channel-notify-props map, a synthetic
    /// `online` status and `allowChannelMentions = true` — so `@channel`, `@all` and `@here` all
    /// count when the user's own notify props allow them, whatever the channel member row says.
    /// Groups come from `getGroupsAllowedForReferenceInChannel`, and a failure there is the same
    /// `app.channel.count_posts_since.app_error` the posts query would raise.
    ///
    /// # The `CreateAt >= timestamp` filter is applied twice
    ///
    /// Once in the store (`GetPostsByThread`'s `since`) and again in the loop. Both inclusive;
    /// the second cannot drop anything the first kept. Kept because Go keeps it.
    #[tracing::instrument(
        skip(self, user, post),
        fields(user_id = %user.id, thread_id = %post.id, team_id = %team_id, timestamp, count)
    )]
    pub async fn count_thread_mentions(
        &self,
        user: &User,
        post: &Post,
        team_id: &str,
        timestamp: i64,
    ) -> AppResult<i64> {
        let count_posts_since_error = |what: &str, err: &dyn std::fmt::Display| {
            tracing::error!(error = %err, "{what} failed while counting thread mentions");
            AppError::boxed(
                "countThreadMentions",
                "app.channel.count_posts_since.app_error",
                None,
                String::new(),
                500,
            )
        };

        let channel = self.get_channel(&post.channel_id).await?;

        let mut keywords = MentionKeywords::new();
        keywords.add_user(
            user,
            &StringMap::new(),
            // Assume the user is online since they would've triggered this.
            Some(&Status {
                status: STATUS_ONLINE.to_owned(),
                ..Default::default()
            }),
            // Assume channel mentions are always allowed for simplicity.
            true,
        );

        let posts = self
            .store()
            .post()
            .get_posts_by_thread(&post.id, timestamp)
            .await
            .map_err(|err| count_posts_since_error("the thread posts query", &err))?;

        if channel.channel_type == CHANNEL_TYPE_DIRECT || channel.channel_type == CHANNEL_TYPE_GROUP
        {
            // In a DM channel, every post made by the other user is a mention.
            let other_id = channel.get_other_user_id_for_dm(&user.id);
            let count = posts
                .iter()
                .filter(|reply| reply.user_id == other_id)
                .count() as i64;
            tracing::Span::current().record("count", count);
            return Ok(count);
        }

        let team = if team_id.is_empty() {
            None
        } else {
            Some(self.get_team(team_id).await?)
        };

        let groups = self
            .get_groups_allowed_for_reference_in_channel(&channel, team.as_ref())
            .await
            .map_err(|err| count_posts_since_error("the group lookup", &err))?;

        keywords.add_groups_map(groups.values());

        let mut count = 0_i64;
        for reply in &posts {
            if reply.create_at >= timestamp {
                let mentions = get_explicit_mentions(reply, &keywords, MM_BLOCKS_ENABLED);
                if mentions.mentions.contains_key(&user.id) {
                    count += 1;
                }
            }
        }

        tracing::Span::current().record("count", count);
        Ok(count)
    }
}
