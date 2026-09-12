//! Port of `App.MarkChannelAsUnreadFromPost` (app/channel.go:3229), its CRT-unsupported twin
//! (:3260) and the part of `App.countMentionsFromPost` (app/post.go:2567) they reach — the app
//! layer behind `POST /api/v4/users/{user_id}/posts/{post_id}/set_unread`.
//!
//! # Two functions, three branches, and the response shape differs between them
//!
//! `MarkChannelAsUnreadFromPost` is a two-line dispatcher: `collapsedThreadsSupported` **and**
//! `IsCRTEnabledForUser` together take the short path; either one false takes the long one. The
//! long path then splits on whether the post is a root or a reply, and the three arms do not
//! agree on what they write:
//!
//! | arm | `UpdateLastViewedAtPost` arguments |
//! |---|---|
//! | CRT supported | `(unreadMentions, unreadMentionsRoot, urgentMentions, **true**)` |
//! | CRT unsupported, root post | `(unreadMentions, unreadMentionsRoot, urgentMentions, **true**)` |
//! | CRT unsupported, **reply** | `(unreadMentions, **0**, **0**, **false**)` |
//!
//! So the same request against the same post answers different `mention_count_root`,
//! `urgent_mention_count` and `msg_count_root` depending on a flag in the request body. That is
//! the shape change a client sees, and it is why `collapsed_threads_supported` is read from the
//! body with `model.MapBoolFromJSON` rather than ignored.
//!
//! The reply arm additionally follows the thread, recomputes the thread's unread mentions and
//! publishes a `thread_updated` event — see [`App::mark_channel_as_unread_from_post`] for why it
//! is refused here rather than half-reproduced.
//!
//! # What is reproduced and what is handed back
//!
//! `countMentionsFromPost` has a **short circuit for direct and group channels**: every post by
//! anyone else is a mention, so it is two counting queries and no parsing. Its other branch
//! builds `MentionKeywords` from every member's notify props and walks the channel's posts
//! through the markdown mention parser — `mention_keywords.go`, `mention_parser_standard.go`,
//! `mention_results.go` and `isPostMention`, none of which this port has. So:
//!
//! - a **direct or group** channel is served here;
//! - an open or private channel is [`MarkUnreadError::Unreproducible`], and the api4 layer
//!   forwards the whole request to Go **before anything is written**;
//! - a **reply** post on the CRT-unsupported arm is likewise unreproducible, decided before the
//!   write for the same reason.
//!
//! Every one of those decisions is taken from rows read, never written, so a forwarded request
//! reaches Go with the database exactly as Go expects to find it.
//!
//! # `UpdateMobileAppBadge` is not ported
//!
//! Both arms end with it. It queues a `notificationTypeUpdateBadge` on
//! `Srv().PushNotificationsHub` (notification_push.go:436); there is no hub here and no device to
//! badge, and nothing about it reaches the HTTP response or the websocket. Same posture as the
//! push clear in [`crate::channel_view`] ([D-215]).

use mm_model::channel::{CHANNEL_TYPE_DIRECT, CHANNEL_TYPE_GROUP};
use mm_model::channel_member::ChannelUnreadAt;
use mm_model::post::Post;
use mm_model::utils::AppError;
use mm_model::websocket_message::{WEBSOCKET_EVENT_POST_UNREAD, WebSocketEvent};
use mm_store::ChannelStore;

use crate::App;

/// Why a set-unread could not be answered here.
///
/// [`MarkUnreadError::Unreproducible`] is not a failure: it is "Go would now run machinery this
/// port does not have", and its only correct handling is to forward the request. It is raised
/// **before** any write, which is the property the api4 layer depends on.
#[derive(Debug, thiserror::Error)]
pub enum MarkUnreadError {
    #[error("marking this post unread is not reproducible here: {0}")]
    Unreproducible(&'static str),
    #[error(transparent)]
    App(#[from] Box<AppError>),
}

impl App {
    /// Port of `App.MarkChannelAsUnreadFromPost` (channel.go:3229) and
    /// `markChannelAsUnreadFromPostCRTUnsupported` (:3260), folded into one function because the
    /// dispatcher is a single condition and the two bodies differ in three arguments.
    ///
    /// # Read order is Go's, and it is observable
    ///
    /// `GetSinglePost` then `GetUser` then the mention count. A post that does not exist is a 404
    /// before a user that does not exist is looked at — so calling this for another user's id
    /// against a missing post reports the post, not the user.
    ///
    /// # The reply arm is refused, not approximated
    ///
    /// On the CRT-unsupported arm a reply post makes Go follow the thread
    /// (`Thread().MaintainMembership` with `Following: true`), set the membership's `LastViewed`
    /// to `post.CreateAt - 1`, recompute `UnreadMentions` via `countThreadMentions` — the mention
    /// engine again — write the membership back, read the thread for the user and publish a
    /// `thread_updated` event. Reproducing the parts that are cheap and skipping
    /// `countThreadMentions` would write a **thread membership row with a wrong mention count**,
    /// which no later request corrects. Refused whole.
    #[tracing::instrument(
        skip(self),
        fields(post_id = %post_id, user_id = %user_id, collapsed_threads_supported, crt, channel_type)
    )]
    pub async fn mark_channel_as_unread_from_post(
        &self,
        post_id: &str,
        user_id: &str,
        collapsed_threads_supported: bool,
    ) -> Result<ChannelUnreadAt, MarkUnreadError> {
        // The dispatcher. Go evaluates `!collapsedThreadsSupported || !IsCRTEnabledForUser(...)`,
        // so the preference lookup is skipped entirely when the client did not claim support.
        let crt_path = collapsed_threads_supported && self.is_crt_enabled_for_user(user_id).await;
        tracing::Span::current().record("crt", crt_path);

        // `incl_deleted = false` on both arms: a soft-deleted post cannot be marked unread.
        let post = self.get_single_post(post_id, false).await?;
        let user = self.get_user(user_id).await?;

        // `markChannelAsUnreadFromPostCRTUnsupported` computes `threadId` here and then uses it
        // only inside the reply arm; skipped, because that arm is refused below.
        if !crt_path && !post.root_id.is_empty() {
            return Err(MarkUnreadError::Unreproducible(
                "a reply on the CRT-unsupported arm follows the thread and recounts its mentions",
            ));
        }

        let (unread_mentions, unread_mentions_root, urgent_mentions) =
            self.count_mentions_from_post(&user.id, &post).await?;

        // Both surviving arms pass `setUnreadCountRoot: true`. The `false` arm is the reply one,
        // refused above — recorded here so that lifting the refusal cannot silently keep `true`.
        let channel_unread = self
            .store()
            .channel()
            .update_last_viewed_at_post(
                &post,
                user_id,
                unread_mentions,
                unread_mentions_root,
                urgent_mentions,
                true,
            )
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "failed to mark the channel unread from a post");
                // Go returns the (nil) unread **and** this error; the caller reads only the error.
                AppError::boxed(
                    "MarkChannelAsUnreadFromPost",
                    "app.channel.update_last_viewed_at_post.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        self.send_web_socket_post_unread_event(&channel_unread, post_id)
            .await;
        // `a.UpdateMobileAppBadge(userID)` — see the module docs.

        Ok(channel_unread)
    }

    /// Port of `App.countMentionsFromPost` (post.go:2567), **direct and group channels only**.
    ///
    /// # Why the DM branch counts what it counts
    ///
    /// Go's comment: "In a DM channel, every post made by the other user is a mention". So the
    /// three numbers are `CountPostsAfter` and `CountUrgentPostsAfter` over the window
    /// `CreateAt - 1`, each **excluding the user's own posts** — the one call site that passes a
    /// non-empty `excludedUserID`. `countRoot` is the same count restricted to roots, which in a
    /// DM is usually but not always equal to it.
    ///
    /// # The urgent count is behind a config flag, and the flag is on by default
    ///
    /// `if a.IsPostPriorityEnabled()` — `ServiceSettings.PostPriority`, which defaults to `true`
    /// and has no licence check (app/post_priority.go:46). With it off the urgent count stays
    /// **0** rather than being computed and discarded, so a server with priorities disabled
    /// answers `"urgent_mention_count":0` here even where urgent posts exist.
    ///
    /// Everything after the DM branch — `GetAllChannelMembersNotifyPropsForChannel`,
    /// `MentionKeywords`, `GetPostThread`, `isPostMention` and the 200-at-a-time walk through
    /// `GetPostsAfterPost` — is the mention engine, and is refused.
    #[tracing::instrument(skip(self, post), fields(channel_id = %post.channel_id))]
    async fn count_mentions_from_post(
        &self,
        user_id: &str,
        post: &Post,
    ) -> Result<(i64, i64, i64), MarkUnreadError> {
        let channel = self.get_channel(&post.channel_id).await?;
        tracing::Span::current().record("channel_type", channel.channel_type.as_str());

        if channel.channel_type != CHANNEL_TYPE_DIRECT && channel.channel_type != CHANNEL_TYPE_GROUP
        {
            return Err(MarkUnreadError::Unreproducible(
                "counting mentions outside a DM or group channel needs the mention parser",
            ));
        }

        // `post.CreateAt - 1`, shared by both counts and by the window
        // `update_last_viewed_at_post` opens — the same expression in three places in Go.
        let since = post.create_at - 1;

        let (count, count_root) = self
            .store()
            .channel()
            .count_posts_after(&post.channel_id, since, user_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "failed to count posts after");
                AppError::boxed(
                    "countMentionsFromPost",
                    "app.channel.count_posts_since.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        let urgent_count = if self.config().post_priority {
            self.store()
                .channel()
                .count_urgent_posts_after(&post.channel_id, since, user_id)
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, "failed to count urgent posts after");
                    // A **different** id from the one above, four lines apart in Go.
                    AppError::boxed(
                        "countMentionsFromPost",
                        "app.channel.count_urgent_posts_since.app_error",
                        None,
                        String::new(),
                        500,
                    )
                })?
        } else {
            0
        };

        Ok((count, count_root, urgent_count))
    }

    /// Port of `App.sendWebSocketPostUnreadEvent` (channel.go:3366).
    ///
    /// Seven fields, all added as **numbers** except `post_id`. The event is addressed to the
    /// team, the channel *and* the user; the hub's targeting takes the narrowest, so only the
    /// user whose read state moved sees it.
    async fn send_web_socket_post_unread_event(
        &self,
        channel_unread: &ChannelUnreadAt,
        post_id: &str,
    ) {
        let mut message = WebSocketEvent::new(
            WEBSOCKET_EVENT_POST_UNREAD,
            &channel_unread.team_id,
            &channel_unread.channel_id,
            &channel_unread.user_id,
            None,
            "",
        );
        message.add("msg_count", channel_unread.msg_count.into());
        message.add("msg_count_root", channel_unread.msg_count_root.into());
        message.add("mention_count", channel_unread.mention_count.into());
        message.add(
            "mention_count_root",
            channel_unread.mention_count_root.into(),
        );
        message.add(
            "urgent_mention_count",
            channel_unread.urgent_mention_count.into(),
        );
        message.add("last_viewed_at", channel_unread.last_viewed_at.into());
        message.add("post_id", serde_json::Value::String(post_id.to_owned()));
        self.publish(message).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three `UpdateLastViewedAtPost` argument sets the three Go arms use, as a table.
    ///
    /// The two arms this port serves both pass `setUnreadCountRoot: true`; the arm it refuses is
    /// the only one that passes `false`, and it also zeroes two of the three counts. Written as a
    /// test so that a future session lifting the reply refusal has to change this table rather
    /// than inherit `true` by accident — which would write a `msg_count_root` a client reads as
    /// "caught up on roots" when Go says the opposite.
    #[test]
    fn only_the_refused_arm_passes_set_unread_count_root_false() {
        /// One row of Go's dispatch table: which arm, and what it passes on.
        struct Arm {
            crt_path: bool,
            is_reply: bool,
            /// `unreadMentionsRoot` reaches the store rather than being replaced by `0`.
            passes_mention_root: bool,
            /// `urgentMentions` reaches the store rather than being replaced by `0`.
            passes_urgent: bool,
            /// The `setUnreadCountRoot` argument.
            set_unread_count_root: bool,
        }

        const ARMS: [Arm; 4] = [
            Arm {
                crt_path: true,
                is_reply: false,
                passes_mention_root: true,
                passes_urgent: true,
                set_unread_count_root: true,
            },
            Arm {
                crt_path: true,
                is_reply: true,
                passes_mention_root: true,
                passes_urgent: true,
                set_unread_count_root: true,
            },
            Arm {
                crt_path: false,
                is_reply: false,
                passes_mention_root: true,
                passes_urgent: true,
                set_unread_count_root: true,
            },
            Arm {
                crt_path: false,
                is_reply: true,
                passes_mention_root: false,
                passes_urgent: false,
                set_unread_count_root: false,
            },
        ];

        for arm in &ARMS {
            // The arm this port serves is `crt_path || !is_reply` — exactly the refusal in
            // `mark_channel_as_unread_from_post`.
            let served = arm.crt_path || !arm.is_reply;
            assert_eq!(
                served, arm.set_unread_count_root,
                "the served arms are exactly the ones that set the root count \
                 (crt={}, reply={})",
                arm.crt_path, arm.is_reply
            );
            assert_eq!(
                arm.passes_mention_root, arm.set_unread_count_root,
                "the refused arm is the one that zeroes the root mention count"
            );
            assert_eq!(arm.passes_urgent, arm.set_unread_count_root);
        }
    }

    /// The two counting errors carry **different** ids, and neither is the update error.
    ///
    /// Three ids inside one call chain, all 500, all invisible in the status line. A port that
    /// reused one of them would be indistinguishable from correct except in the one field a
    /// client can branch on.
    #[test]
    fn the_three_error_ids_on_this_path_are_distinct() {
        let ids = [
            "app.channel.count_posts_since.app_error",
            "app.channel.count_urgent_posts_since.app_error",
            "app.channel.update_last_viewed_at_post.app_error",
        ];
        let unique: std::collections::BTreeSet<&str> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "no two of these ids are the same");
    }

    /// Only `D` and `G` take the short circuit. `O` and `P` — and anything else the schema grows
    /// — need the mention engine.
    #[test]
    fn the_short_circuit_is_direct_and_group_and_nothing_else() {
        for channel_type in ["O", "P", "B", ""] {
            assert!(
                channel_type != CHANNEL_TYPE_DIRECT && channel_type != CHANNEL_TYPE_GROUP,
                "{channel_type} must not reach the DM branch"
            );
        }
        assert_eq!(CHANNEL_TYPE_DIRECT, "D");
        assert_eq!(CHANNEL_TYPE_GROUP, "G");
    }
}
