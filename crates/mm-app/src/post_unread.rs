//! Port of `App.MarkChannelAsUnreadFromPost` (app/channel.go:3229), its CRT-unsupported twin
//! (:3260) and `App.countMentionsFromPost` (app/post.go:2567) — the app layer behind
//! `POST /api/v4/users/{user_id}/posts/{post_id}/set_unread`.
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
//! # The reply arm follows the thread first
//!
//! On the CRT-unsupported arm a reply makes Go, when `ThreadAutoFollow` is on, follow the thread
//! (creating the membership if there is none), move its `LastViewed` to `post.CreateAt - 1`,
//! recount its `UnreadMentions` with `countThreadMentions` and write it back — and, only when the
//! user has collapsed threads on, publish the thread as a `thread_updated` event. See
//! [`App::follow_thread_for_unread_reply`]. Every failure in there, the extended thread read
//! included, is the one `app.channel.update_last_viewed_at_post.app_error`.
//!
//! # The mention count ([D-421], closed)
//!
//! `countMentionsFromPost` short-circuits direct and group channels: every post by anyone else is
//! a mention. Everywhere else it builds the user's [`MentionKeywords`] (with the member's channel
//! notify props, a synthetic online status and channel mentions allowed) and walks the post, then
//! every later post 200 at a time, through `isPostMention` — [`App::is_post_mention`], on the
//! same [`crate::mention`] parser the notification pass uses. Root mentions whose priority is
//! `urgent` are the urgent count.
//!
//! # `UpdateMobileAppBadge`
//!
//! Every arm ends with it: an `update_badge` push to every device of the user
//! ([`crate::push`]), which nothing in the response waits for.

use std::collections::HashMap;

use mm_model::channel::{CHANNEL_TYPE_DIRECT, CHANNEL_TYPE_GROUP};
use mm_model::channel_member::ChannelUnreadAt;
use mm_model::post::{
    POST_PRIORITY_URGENT, POST_PROPS_ADDED_USER_ID, POST_PROPS_FROM_WEBHOOK,
    POST_TYPE_ADD_TO_CHANNEL, Post,
};
use mm_model::post_list::PostMap;
use mm_model::status::{STATUS_ONLINE, Status};
use mm_model::user::{COMMENTS_NOTIFY_ANY, COMMENTS_NOTIFY_PROP, COMMENTS_NOTIFY_ROOT, User};
use mm_model::utils::{AppError, StringMap, go_json_marshal};
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_POST_UNREAD, WEBSOCKET_EVENT_THREAD_UPDATED, WebSocketEvent,
};
use mm_store::ChannelStore;
use mm_store::post_store::{
    GetPostThreadOptions, GetPostsAroundOptions, PostStore, ThreadDirection,
};
use mm_store::thread_store::{ThreadMembershipOpts, ThreadStore};

use crate::App;
use crate::mention::MentionKeywords;

/// Why a set-unread could not be answered here.
///
/// Every arm is served since [D-421] closed; the variant stays for the api4 layer's forward, which
/// a future arm that needs machinery this port lacks would use again. It is raised **before** any
/// write, which is the property the api4 layer depends on.
#[derive(Debug, thiserror::Error)]
pub enum MarkUnreadError {
    #[error("marking this post unread is not reproducible here: {0}")]
    Unreproducible(&'static str),
    #[error(transparent)]
    App(#[from] Box<AppError>),
}

/// `app.channel.update_last_viewed_at_post.app_error`, the one id of every failure in the
/// reply arm and of the final write.
fn update_last_viewed_error(what: &str, err: &dyn std::fmt::Display) -> Box<AppError> {
    tracing::error!(error = %err, "{what} failed while marking the channel unread from a post");
    AppError::boxed(
        "MarkChannelAsUnreadFromPost",
        "app.channel.update_last_viewed_at_post.app_error",
        None,
        String::new(),
        500,
    )
}

impl App {
    /// Port of `App.MarkChannelAsUnreadFromPost` (channel.go:3229) and
    /// `markChannelAsUnreadFromPostCRTUnsupported` (:3260), folded into one function because the
    /// dispatcher is a single condition and the bodies differ only in the reply arm.
    ///
    /// # Read order is Go's, and it is observable
    ///
    /// `GetSinglePost` then `GetUser` then the mention count. A post that does not exist is a 404
    /// before a user that does not exist is looked at — so calling this for another user's id
    /// against a missing post reports the post, not the user.
    ///
    /// `session_user_id` is `rctx.Session().UserId`, which `countMentionsFromPost` hands
    /// `GetPostsAfterPost` as the reading user — the caller, not the user being marked.
    #[tracing::instrument(
        skip(self, ctx),
        fields(post_id = %post_id, user_id = %user_id, collapsed_threads_supported, crt, channel_type)
    )]
    pub async fn mark_channel_as_unread_from_post(
        &self,
        ctx: &crate::plugin_hooks::HookContext,
        session_user_id: &str,
        post_id: &str,
        user_id: &str,
        collapsed_threads_supported: bool,
    ) -> Result<ChannelUnreadAt, MarkUnreadError> {
        // The dispatcher. Go evaluates `!collapsedThreadsSupported || !IsCRTEnabledForUser(...)`,
        // so the preference lookup is skipped entirely when the client did not claim support.
        let crt_path = collapsed_threads_supported && self.is_crt_enabled_for_user(user_id).await;
        tracing::Span::current().record("crt", crt_path);

        // `incl_deleted = false` on both arms: a soft-deleted post cannot be marked unread.
        let post = self.get_single_post(ctx, post_id, false).await?;
        let user = self.get_user(user_id).await?;

        let (unread_mentions, unread_mentions_root, urgent_mentions) = self
            .count_mentions_from_post(ctx, session_user_id, &user, &post)
            .await?;

        // The CRT arm and the CRT-unsupported root arm write the same four arguments; the reply
        // arm follows the thread and then writes `(unreadMentions, 0, 0, false)`.
        let is_reply = !post.root_id.is_empty();
        if !crt_path && is_reply {
            self.follow_thread_for_unread_reply(ctx, &user, &post)
                .await?;
        }
        let (mentions_root, urgent, set_unread_count_root) =
            unread_at_post_arguments(crt_path, is_reply, unread_mentions_root, urgent_mentions);

        let channel_unread = self
            .store()
            .channel()
            .update_last_viewed_at_post(
                &post,
                user_id,
                unread_mentions,
                mentions_root,
                urgent,
                set_unread_count_root,
            )
            .await
            // Go returns the (nil) unread **and** this error; the caller reads only the error.
            .map_err(|err| update_last_viewed_error("UpdateLastViewedAtPost", &err))?;

        self.send_web_socket_post_unread_event(&channel_unread, post_id)
            .await;
        self.update_mobile_app_badge(user_id);

        Ok(channel_unread)
    }

    /// The reply half of `markChannelAsUnreadFromPostCRTUnsupported` (channel.go:3300-3354):
    /// the root post, the channel, and — with `ThreadAutoFollow` on — the thread membership.
    ///
    /// A membership that exists is reused **whatever its `Following`**, then set to follow: "if
    /// threadmembership already exists but user had previously unfollowed the thread, then
    /// follow the thread again". One that does not is created by `MaintainMembership` with
    /// `Following` and `UpdateFollowing` only, so its `LastViewed` starts at zero and its
    /// participants are untouched. `UnreadMentions` is then `countThreadMentions` from the
    /// **root** post, over the replies from `post.CreateAt - 1` on.
    ///
    /// The thread is read back **extended** (real participant profiles, sanitised as
    /// `sanitizeProfiles(…, false)`) and published only when the user has collapsed threads on —
    /// a client that did not claim CRT support may still belong to a user who has it.
    async fn follow_thread_for_unread_reply(
        &self,
        ctx: &crate::plugin_hooks::HookContext,
        user: &User,
        post: &Post,
    ) -> Result<(), MarkUnreadError> {
        let thread_id = post.root_id.as_str();
        let root_post = self.get_single_post(ctx, thread_id, false).await?;
        let channel = self
            .store()
            .channel()
            .get(&post.channel_id)
            .await
            .map_err(|err| update_last_viewed_error("the channel read", &err))?;

        if !self.config().thread_auto_follow {
            return Ok(());
        }

        let existing = match self
            .store()
            .thread()
            .get_membership_for_user(&user.id, thread_id)
            .await
        {
            Ok(membership) => Some(membership),
            Err(err) if err.is_not_found() => None,
            Err(err) => return Err(update_last_viewed_error("the membership read", &err).into()),
        };
        let mut membership = match existing {
            Some(membership) => membership,
            None => self
                .store()
                .thread()
                .maintain_membership(
                    &user.id,
                    thread_id,
                    ThreadMembershipOpts {
                        following: true,
                        update_following: true,
                        ..ThreadMembershipOpts::default()
                    },
                )
                .await
                .map_err(|err| update_last_viewed_error("MaintainMembership", &err))?,
        };

        membership.following = true;
        membership.last_viewed = post.create_at - 1;
        membership.unread_mentions = self
            .count_thread_mentions(user, &root_post, &channel.team_id, post.create_at - 1)
            .await?;
        self.store()
            .thread()
            .update_membership(&membership)
            .await
            .map_err(|err| update_last_viewed_error("UpdateMembership", &err))?;

        let mut thread = self
            .get_thread_for_user(&membership, true)
            .await
            .map_err(|err| update_last_viewed_error("GetThreadForUser", &err))?;
        // `sanitizeThreadResponse`: the participants here; the post's props and action
        // integrations were already handled by `get_thread_for_user`.
        let options = self.sanitize_options(false);
        for participant in thread.participants.iter_mut().flatten() {
            participant.sanitize_profile(&options, false);
        }

        if self.is_crt_enabled_for_user(&user.id).await {
            let payload = go_json_marshal(&thread).map_err(|err| {
                tracing::error!(error = %err, "could not encode the thread");
                AppError::boxed(
                    "MarkChannelAsUnreadFromPost",
                    "api.marshal_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
            let mut message = WebSocketEvent::new(
                WEBSOCKET_EVENT_THREAD_UPDATED,
                &channel.team_id,
                "",
                &user.id,
                None,
                "",
            );
            message.add("thread", serde_json::Value::String(payload));
            self.publish(message).await;
        }
        Ok(())
    }

    /// Port of `App.countMentionsFromPost` (post.go:2567).
    ///
    /// # Direct and group channels count without parsing
    ///
    /// Go's comment: "In a DM channel, every post made by the other user is a mention". So the
    /// three numbers are `CountPostsAfter` and `CountUrgentPostsAfter` over the window
    /// `CreateAt - 1`, each **excluding the user's own posts**.
    ///
    /// # Everywhere else, `isPostMention` over the post and everything after it
    ///
    /// The post itself is checked against its own thread (`GetPostThread`), then every later
    /// post in pages of 200 (`GetPostsAfterPost`, read as the session's user) against that page.
    /// `mentionedByThread` is shared across the whole walk, so a thread's comment-mention answer
    /// is decided once, by the first reply seen in it. A root mention is urgent when its priority
    /// says so — one `GetPriorityForPost` for the post itself, one `PostPriority().GetForPosts`
    /// per page for the rest — and only while `ServiceSettings.PostPriority` is on.
    #[tracing::instrument(skip(self, ctx, user, post), fields(channel_id = %post.channel_id))]
    async fn count_mentions_from_post(
        &self,
        ctx: &crate::plugin_hooks::HookContext,
        session_user_id: &str,
        user: &User,
        post: &Post,
    ) -> Result<(i64, i64, i64), MarkUnreadError> {
        let count_posts_since_error = |err: &dyn std::fmt::Display| {
            tracing::error!(error = %err, "failed to count posts after");
            AppError::boxed(
                "countMentionsFromPost",
                "app.channel.count_posts_since.app_error",
                None,
                String::new(),
                500,
            )
        };

        let channel = self.get_channel(&post.channel_id).await?;
        tracing::Span::current().record("channel_type", channel.channel_type.as_str());

        // `post.CreateAt - 1`, shared by both counts and by the window
        // `update_last_viewed_at_post` opens — the same expression in three places in Go.
        let since = post.create_at - 1;

        if channel.channel_type == CHANNEL_TYPE_DIRECT || channel.channel_type == CHANNEL_TYPE_GROUP
        {
            let (count, count_root) = self
                .store()
                .channel()
                .count_posts_after(&post.channel_id, since, &user.id)
                .await
                .map_err(|err| count_posts_since_error(&err))?;

            let urgent_count = if self.config().post_priority {
                self.store()
                    .channel()
                    .count_urgent_posts_after(&post.channel_id, since, &user.id)
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

            return Ok((count, count_root, urgent_count));
        }

        let members = self
            .store()
            .channel()
            .get_all_channel_members_notify_props_for_channel(&channel.id, true)
            .await
            .map_err(|err| count_posts_since_error(&err))?;

        let mut keywords = MentionKeywords::new();
        keywords.add_user(
            user,
            members.get(&user.id).unwrap_or(&StringMap::new()),
            // Assume the user is online since they would've triggered this.
            Some(&Status {
                status: STATUS_ONLINE.to_owned(),
                ..Default::default()
            }),
            // Assume channel mentions are always allowed for simplicity.
            true,
        );
        let comment_mentions = user
            .notify_props
            .as_ref()
            .and_then(|props| props.get(COMMENTS_NOTIFY_PROP))
            .map_or("", String::as_str);
        let check_for_comment_mentions =
            comment_mentions == COMMENTS_NOTIFY_ROOT || comment_mentions == COMMENTS_NOTIFY_ANY;

        // A mapping of thread root ids to whether a post in that thread mentions the user.
        let mut mentioned_by_thread: HashMap<String, bool> = HashMap::new();

        let thread = self
            .get_post_thread(
                ctx,
                &post.id,
                GetPostThreadOptions {
                    user_id: &user.id,
                    skip_fetch_threads: false,
                    collapsed_threads: false,
                    updates_only: false,
                    per_page: 0,
                    direction: ThreadDirection::Unset,
                    from_post: "",
                    from_create_at: 0,
                    from_update_at: 0,
                },
            )
            .await?;
        let empty = PostMap::new();

        let mut count = 0_i64;
        let mut count_root = 0_i64;
        let mut urgent_count = 0_i64;
        if self.is_post_mention(
            user,
            post,
            &keywords,
            thread.posts.as_ref().unwrap_or(&empty),
            &mut mentioned_by_thread,
            check_for_comment_mentions,
        ) {
            count += 1;
            if post.root_id.is_empty() {
                count_root += 1;
                if self.config().post_priority {
                    let priority = self
                        .store()
                        .post()
                        .get_priority_for_post(&post.id)
                        .await
                        .map_err(|err| {
                            tracing::error!(error = %err, "failed to read the post's priority");
                            AppError::boxed(
                                "GetPriorityForPost",
                                // Go's spelling.
                                "app.post_prority.get_for_post.app_error",
                                None,
                                String::new(),
                                500,
                            )
                        })?;
                    if priority.is_some_and(|p| p.priority.as_deref() == Some(POST_PRIORITY_URGENT))
                    {
                        urgent_count += 1;
                    }
                }
            }
        }

        const PER_PAGE: i64 = 200;
        let mut page = 0_i64;
        loop {
            let list = self
                .get_posts_around_post(
                    ctx,
                    GetPostsAroundOptions {
                        channel_id: &post.channel_id,
                        post_id: &post.id,
                        user_id: session_user_id,
                        page,
                        per_page: PER_PAGE,
                        skip_fetch_threads: false,
                        collapsed_threads: false,
                    },
                    false,
                )
                .await?;
            let order = list.order.as_deref().unwrap_or_default();
            let posts = list.posts.as_ref().unwrap_or(&empty);

            let mut mention_post_ids: Vec<String> = Vec::new();
            for id in order {
                let Some(candidate) = posts.get(id) else {
                    // Go indexes the map and would dereference nil here; an order entry without
                    // its post does not occur on this query.
                    continue;
                };
                if self.is_post_mention(
                    user,
                    candidate,
                    &keywords,
                    posts,
                    &mut mentioned_by_thread,
                    check_for_comment_mentions,
                ) {
                    count += 1;
                    if candidate.root_id.is_empty() {
                        mention_post_ids.push(id.clone());
                        count_root += 1;
                    }
                }
            }

            if self.config().post_priority {
                let priorities = self
                    .store()
                    .post()
                    .get_priority_for_posts(&mention_post_ids)
                    .await
                    .map_err(|err| {
                        tracing::error!(error = %err, "failed to read the priorities");
                        AppError::boxed(
                            "countMentionsFromPost",
                            "app.channel.get_priority_for_posts.app_error",
                            None,
                            String::new(),
                            500,
                        )
                    })?;
                urgent_count += priorities
                    .iter()
                    .filter(|p| p.priority.as_deref() == Some(POST_PRIORITY_URGENT))
                    .count() as i64;
            }

            if (order.len() as i64) < PER_PAGE {
                break;
            }
            page += 1;
        }

        Ok((count, count_root, urgent_count))
    }

    /// Port of `isPostMention` (post.go:2724).
    ///
    /// In order: the user's own post is never a mention **unless it came from a webhook** (the
    /// prop must be the string `"true"`); an explicit mention by keyword; a
    /// `system_add_to_channel` post naming the user as `addedUserId`; and, when the user's
    /// `comments` notify prop is `root` or `any`, [`is_comment_mention`].
    pub(crate) fn is_post_mention(
        &self,
        user: &User,
        post: &Post,
        keywords: &MentionKeywords,
        other_posts: &PostMap,
        mentioned_by_thread: &mut HashMap<String, bool>,
        check_for_comment_mentions: bool,
    ) -> bool {
        let from_webhook = post.get_prop(POST_PROPS_FROM_WEBHOOK)
            == Some(&serde_json::Value::String("true".to_owned()));
        if post.user_id == user.id && !from_webhook {
            return false;
        }

        if self
            .explicit_mentions(post, keywords)
            .mentions
            .contains_key(&user.id)
        {
            return true;
        }

        if post.post_type == POST_TYPE_ADD_TO_CHANNEL
            && post
                .get_prop(POST_PROPS_ADDED_USER_ID)
                .and_then(|v| v.as_str())
                == Some(user.id.as_str())
        {
            return true;
        }

        check_for_comment_mentions
            && is_comment_mention(user, post, other_posts, mentioned_by_thread)
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

/// The last three `UpdateLastViewedAtPost` arguments of each arm — the module docs' table. Only
/// the CRT-unsupported reply arm zeroes the root and urgent counts and passes
/// `setUnreadCountRoot = false`.
fn unread_at_post_arguments(
    crt_path: bool,
    is_reply: bool,
    unread_mentions_root: i64,
    urgent_mentions: i64,
) -> (i64, i64, bool) {
    if crt_path || !is_reply {
        (unread_mentions_root, urgent_mentions, true)
    } else {
        (0, 0, false)
    }
}

/// Port of `isCommentMention` (post.go:2677): whether a **reply** mentions the user by being in a
/// thread they started or — with `comments = any` — commented on earlier.
///
/// The answer is per thread and cached in `mentioned_by_thread` by the first reply that asks, so
/// a later reply in the same thread gets the same answer whatever its own timestamp. A root
/// missing from `other_posts` is "past the cloud plan's limit" in Go's words and not a mention;
/// that answer is **not** cached.
fn is_comment_mention(
    user: &User,
    post: &Post,
    other_posts: &PostMap,
    mentioned_by_thread: &mut HashMap<String, bool>,
) -> bool {
    if post.root_id.is_empty() {
        // Not a comment.
        return false;
    }
    if let Some(mentioned) = mentioned_by_thread.get(&post.root_id) {
        return *mentioned;
    }
    let Some(root) = other_posts.get(&post.root_id) else {
        tracing::warn!(
            root_post_id = %post.root_id,
            comment_id = %post.id,
            "Can't determine the comment mentions as the rootPost is past the cloud plan's limit"
        );
        return false;
    };

    // Whether the user started the thread, or commented on it before this post.
    let mut mentioned = root.user_id == user.id;
    let comments = user
        .notify_props
        .as_ref()
        .and_then(|props| props.get(COMMENTS_NOTIFY_PROP))
        .map_or("", String::as_str);
    if !mentioned && comments == COMMENTS_NOTIFY_ANY {
        mentioned = other_posts.values().any(|other| {
            other.id != post.id
                && other.root_id == post.root_id
                && other.user_id == user.id
                && other.create_at < post.create_at
        });
    }

    mentioned_by_thread.insert(post.root_id.clone(), mentioned);
    mentioned
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three `UpdateLastViewedAtPost` argument sets the three Go arms use, as a table: only
    /// the CRT-unsupported reply zeroes the root and urgent counts and passes `false`.
    #[test]
    fn only_the_crt_unsupported_reply_passes_set_unread_count_root_false() {
        assert_eq!(unread_at_post_arguments(true, false, 3, 2), (3, 2, true));
        assert_eq!(unread_at_post_arguments(true, true, 3, 2), (3, 2, true));
        assert_eq!(unread_at_post_arguments(false, false, 3, 2), (3, 2, true));
        assert_eq!(unread_at_post_arguments(false, true, 3, 2), (0, 0, false));
    }

    fn post(id: &str, root_id: &str, user_id: &str, create_at: i64) -> Post {
        Post {
            id: id.to_owned(),
            root_id: root_id.to_owned(),
            user_id: user_id.to_owned(),
            create_at,
            ..Post::default()
        }
    }

    fn user_with_comments(id: &str, comments: &str) -> User {
        User {
            id: id.to_owned(),
            notify_props: Some(
                [(COMMENTS_NOTIFY_PROP.to_owned(), comments.to_owned())]
                    .into_iter()
                    .collect(),
            ),
            ..User::default()
        }
    }

    /// Every branch of `isCommentMention` (post.go:2677), including the per-thread cache.
    #[test]
    fn a_comment_mention_is_decided_once_per_thread_as_go_decides_it() {
        let me = "u1";
        let other = "u2";
        let mut posts = PostMap::new();
        for p in [
            post("root", "", me, 10),
            post("root2", "", other, 10),
            post("r1", "root2", me, 20),
            post("r2", "root2", other, 30),
            post("r0", "root2", other, 15),
        ] {
            posts.insert(p.id.clone(), p);
        }
        let root_only = user_with_comments(me, COMMENTS_NOTIFY_ROOT);
        let any = user_with_comments(me, COMMENTS_NOTIFY_ANY);

        // A root is never a comment mention.
        assert!(!is_comment_mention(
            &any,
            &posts["root"],
            &posts,
            &mut HashMap::new()
        ));
        // A reply in a thread the user started.
        let started = post("x", "root", other, 50);
        assert!(is_comment_mention(
            &root_only,
            &started,
            &posts,
            &mut HashMap::new()
        ));
        // `root` does not count earlier comments; `any` counts only **earlier** ones.
        assert!(!is_comment_mention(
            &root_only,
            &posts["r2"],
            &posts,
            &mut HashMap::new()
        ));
        assert!(is_comment_mention(
            &any,
            &posts["r2"],
            &posts,
            &mut HashMap::new()
        ));
        assert!(!is_comment_mention(
            &any,
            &posts["r0"],
            &posts,
            &mut HashMap::new()
        ));
        // The first answer for a thread sticks for every later reply in it.
        let mut cache = HashMap::new();
        assert!(!is_comment_mention(&any, &posts["r0"], &posts, &mut cache));
        assert!(!is_comment_mention(&any, &posts["r2"], &posts, &mut cache));
        // A root outside the page is not a mention, and is not cached.
        let orphan = post("y", "gone", other, 60);
        let mut cache = HashMap::new();
        assert!(!is_comment_mention(&any, &orphan, &posts, &mut cache));
        assert!(cache.is_empty());
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
