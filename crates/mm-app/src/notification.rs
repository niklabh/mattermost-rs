//! Port of the notification pass behind a created post: `App.handlePostEvents`
//! (app/post.go), `App.SendNotifications` (app/notification.go:53) and the helpers it reads —
//! `getExplicitMentionsAndKeywords`, `allowChannelMentions`, `allowGroupMentions`,
//! `getMentionKeywordsInChannel`, `CRTNotifiers`, `shouldUserNotifyCRT`,
//! `shouldChannelMemberNotifyCRT`, `shouldAckWebsocketNotification` and `PostNotification`.
//!
//! # What of `SendNotifications` is here, and what is not
//!
//! Everything a **database or a websocket** can see: the mention pass over the channel's
//! members, the thread auto-follow writes and the participants update, `IncrementMentionCount`,
//! the `posted` event with its three broadcast hooks, and the per-follower `thread_updated`
//! events. Email and push are external and gated on settings this server does not act on
//! ([D-402]); the mobile all-activity list is computed by Go only to feed push, so it is not
//! built here.
//!
//! Three arms are **forwarded before the row is written** rather than reproduced, because each
//! ends in text this server cannot mint — see [`App::notification_forward_reason`]:
//! a group mention (`insertGroupMentions` and the member lists), an out-of-channel mention (an
//! ephemeral notice whose message is translated, [D-092]) and a channel-wide mention in a channel
//! over `MaxNotificationsPerChannel` (the same). The forward runs the same mention pass the
//! fan-out runs, on the same inputs, so a post that clears it is one the fan-out serves whole.
//!
//! # `@here` reads Go's status cache, and this server has none
//!
//! `getMentionKeywordsInChannel` asks `GetStatusFromCache`, which answers **nil** on a miss — so
//! a member whose status Go has not cached since boot cannot be `@here`-mentioned there, however
//! online the `Status` row says they are. This port reads the row. Both servers agree whenever
//! the member's status has been read or written since the Go server started, which is what every
//! connected client causes; the divergence is the [D-087] class and is recorded in
//! `MIGRATION.md` rather than opened as debt.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use mm_model::channel::{CHANNEL_TYPE_DIRECT, CHANNEL_TYPE_GROUP, Channel};
use mm_model::channel_member::{
    CHANNEL_AUTO_FOLLOW_THREADS, CHANNEL_AUTO_FOLLOW_THREADS_ON, CHANNEL_NOTIFY_ALL,
    CHANNEL_NOTIFY_DEFAULT, CHANNEL_NOTIFY_MENTION, CHANNEL_NOTIFY_NONE,
};
use mm_model::config::COLLAPSED_THREADS_DISABLED;
use mm_model::group::Group;
use mm_model::license::minimum_professional_license;
use mm_model::permission::{PERMISSION_USE_CHANNEL_MENTIONS, PERMISSION_USE_GROUP_MENTIONS};
use mm_model::post::{
    POST_PROPS_ADDED_USER_ID, POST_PROPS_FROM_WEBHOOK, POST_PROPS_OVERRIDE_USERNAME,
    POST_TYPE_ADD_TO_CHANNEL, POST_TYPE_HEADER_CHANGE, POST_TYPE_PURPOSE_CHANGE, Post,
};
use mm_model::post_list::PostList;
use mm_model::status::Status;
use mm_model::team::Team;
use mm_model::thread::ThreadMembership;
use mm_model::user::external::SHOW_USERNAME;
use mm_model::user::{
    CHANNEL_MENTION_AUTO_FOLLOW_THREADS_PROP, COMMENTS_NOTIFY_ANY, COMMENTS_NOTIFY_PROP,
    COMMENTS_NOTIFY_ROOT, DESKTOP_NOTIFY_PROP, DESKTOP_THREADS_NOTIFY_PROP, EMAIL_NOTIFY_PROP,
    EMAIL_THREADS_NOTIFY_PROP, PUSH_NOTIFY_PROP, PUSH_THREADS_NOTIFY_PROP, USER_NOTIFY_ALL,
    USER_NOTIFY_MENTION, USER_NOTIFY_NONE, User,
};
use mm_model::utils::{AppError, AppResult, StringInterface, StringMap, go_json_marshal};
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_POSTED, WEBSOCKET_EVENT_THREAD_UPDATED, WebSocketEvent,
};
use mm_store::thread_store::ThreadMembershipOpts;
use mm_store::{ChannelStore, StatusStore, ThreadStore, UserStore};

use crate::App;
use crate::broadcast_hooks::{
    BROADCAST_ADD_FOLLOWERS, BROADCAST_ADD_MENTIONS, BROADCAST_POSTED_ACK,
};
use crate::mention::{MentionKeywords, MentionResults, MentionType, get_explicit_mentions};
use crate::thread_read::MM_BLOCKS_ENABLED;

/// Port of `app.CRTNotifiers` (notification.go): the followers of a thread who should be told
/// about a reply, by channel. Only `desktop` reaches the wire here — it is the `add_followers`
/// hook's list — but all three are computed because they are one decision table.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CrtNotifiers {
    pub desktop: Vec<String>,
    pub email: Vec<String>,
    pub push: Vec<String>,
}

impl CrtNotifiers {
    /// Port of `(*CRTNotifiers).addFollowerToNotify` (notification.go:1683).
    ///
    /// The user's global notify props decide when the channel member's props say `default`;
    /// otherwise the member's props decide — for desktop and push. Email has no per-channel
    /// setting and is the user's alone.
    pub fn add_follower_to_notify(
        &mut self,
        user: &User,
        mentions: &MentionResults,
        channel_member_props: &StringMap,
        _channel: &Channel,
    ) {
        let user_was_mentioned = mentions.mentions.contains_key(&user.id);
        let (notify_desktop, notify_push, notify_email) =
            should_user_notify_crt(user, user_was_mentioned);
        let (notify_channel_desktop, notify_channel_push) = should_channel_member_notify_crt(
            user.notify_props.as_ref(),
            channel_member_props,
            user_was_mentioned,
        );

        let member = |key: &str| channel_member_props.get(key).map_or("", String::as_str);

        if (member(DESKTOP_NOTIFY_PROP) == CHANNEL_NOTIFY_DEFAULT && notify_desktop)
            || notify_channel_desktop
        {
            self.desktop.push(user.id.clone());
        }
        if notify_email {
            self.email.push(user.id.clone());
        }
        if (member(PUSH_NOTIFY_PROP) == CHANNEL_NOTIFY_DEFAULT && notify_push)
            || notify_channel_push
        {
            self.push.push(user.id.clone());
        }
    }
}

fn user_prop<'a>(props: Option<&'a StringMap>, key: &str) -> &'a str {
    props.and_then(|p| p.get(key)).map_or("", String::as_str)
}

/// Port of `shouldUserNotifyCRT` (notification.go:1706): `(desktop, push, email)`.
pub fn should_user_notify_crt(user: &User, is_mentioned: bool) -> (bool, bool, bool) {
    let props = user.notify_props.as_ref();
    let desktop = user_prop(props, DESKTOP_NOTIFY_PROP);
    let push = user_prop(props, PUSH_NOTIFY_PROP);
    let should_email = user_prop(props, EMAIL_NOTIFY_PROP) == "true";
    let desktop_threads = user_prop(props, DESKTOP_THREADS_NOTIFY_PROP);
    let email_threads = user_prop(props, EMAIL_THREADS_NOTIFY_PROP);
    let push_threads = user_prop(props, PUSH_THREADS_NOTIFY_PROP);

    let notify_desktop = desktop != USER_NOTIFY_NONE
        && (is_mentioned || desktop_threads == USER_NOTIFY_ALL || desktop == USER_NOTIFY_ALL);
    let notify_email = should_email && (is_mentioned || email_threads == USER_NOTIFY_ALL);
    let notify_push = push != USER_NOTIFY_NONE
        && (is_mentioned || push_threads == USER_NOTIFY_ALL || push == USER_NOTIFY_ALL);
    (notify_desktop, notify_push, notify_email)
}

/// Port of `shouldChannelMemberNotifyCRT` (notification.go:1741): `(desktop, push)`.
///
/// Note the asymmetry Go carries: the desktop arm also consults the **user's** `desktop_threads`
/// (`!= mention`), the push arm does not.
pub fn should_channel_member_notify_crt(
    user_props: Option<&StringMap>,
    member_props: &StringMap,
    is_mentioned: bool,
) -> (bool, bool) {
    let member = |key: &str| member_props.get(key).map_or("", String::as_str);
    let desktop = member(DESKTOP_NOTIFY_PROP);
    let push = member(PUSH_NOTIFY_PROP);
    let desktop_threads = member(DESKTOP_THREADS_NOTIFY_PROP);
    let user_desktop_threads = user_prop(user_props, DESKTOP_THREADS_NOTIFY_PROP);
    let push_threads = member(PUSH_THREADS_NOTIFY_PROP);

    let notify_desktop = desktop != CHANNEL_NOTIFY_DEFAULT
        && desktop != CHANNEL_NOTIFY_NONE
        && (is_mentioned
            || (desktop_threads == CHANNEL_NOTIFY_ALL
                && user_desktop_threads != USER_NOTIFY_MENTION)
            || desktop == CHANNEL_NOTIFY_ALL);
    let notify_push = push != CHANNEL_NOTIFY_DEFAULT
        && push != CHANNEL_NOTIFY_NONE
        && (is_mentioned || push_threads == CHANNEL_NOTIFY_ALL || push == CHANNEL_NOTIFY_ALL);
    (notify_desktop, notify_push)
}

/// Port of `shouldAckWebsocketNotification` (notification.go:1767): whether a member's
/// connection is asked to acknowledge the `posted` event, from the channel's notify level with
/// the user's as the fallback for `default`, plus the group-message rule.
pub fn should_ack_websocket_notification(
    channel_type: &str,
    user_notification_level: &str,
    channel_notification_level: &str,
) -> bool {
    // Should ACK if we notify for all messages in the channel, or for all messages with the
    // channel settings unchanged, or in a group channel where the default settings are in place.
    channel_notification_level == CHANNEL_NOTIFY_ALL
        || (channel_notification_level == CHANNEL_NOTIFY_DEFAULT
            && user_notification_level == USER_NOTIFY_ALL)
        || (channel_type == CHANNEL_TYPE_GROUP
            && ((channel_notification_level == CHANNEL_NOTIFY_DEFAULT
                && user_notification_level == USER_NOTIFY_MENTION)
                || channel_notification_level == CHANNEL_NOTIFY_MENTION))
}

/// Port of `app.PostNotification` (notification.go:1606) — the names the `posted` event and the
/// email/push templates are built from.
pub struct PostNotification<'a> {
    pub post: &'a Post,
    pub channel: &'a Channel,
    pub profile_map: &'a BTreeMap<String, User>,
    pub sender: &'a User,
}

impl PostNotification<'_> {
    /// Port of `(*PostNotification).GetChannelName` (notification.go:1621). A DM is the sender's
    /// name with an `@`; a GM is every member's display name but `exclude_id`, **sorted**; anything
    /// else is the channel's display name untouched.
    pub fn get_channel_name(&self, user_name_format: &str, exclude_id: &str) -> String {
        match self.channel.channel_type.as_str() {
            CHANNEL_TYPE_DIRECT => self
                .sender
                .get_display_name_with_prefix(user_name_format, "@"),
            CHANNEL_TYPE_GROUP => {
                let mut names: Vec<String> = self
                    .profile_map
                    .values()
                    .filter(|user| user.id != exclude_id)
                    .map(|user| user.get_display_name(user_name_format))
                    .collect();
                names.sort();
                names.join(", ")
            }
            _ => self.channel.display_name.clone(),
        }
    }

    /// Port of `(*PostNotification).GetSenderName` (notification.go:1643).
    ///
    /// A system message's sender is the translated `system.message.name`, which this server
    /// cannot mint ([D-092]); no system post reaches this pass from the create route. The
    /// webhook override is honoured only outside a DM and only when the post says `from_webhook`.
    pub fn get_sender_name(&self, user_name_format: &str, overrides_allowed: bool) -> String {
        if overrides_allowed && self.channel.channel_type != CHANNEL_TYPE_DIRECT {
            if let Some(value) = self.post.get_prop(POST_PROPS_OVERRIDE_USERNAME) {
                if self
                    .post
                    .get_prop(POST_PROPS_FROM_WEBHOOK)
                    .and_then(|v| v.as_str())
                    == Some("true")
                {
                    if let Some(name) = value.as_str() {
                        return name.to_owned();
                    }
                }
            }
        }
        self.sender
            .get_display_name_with_prefix(user_name_format, "@")
    }
}

/// What the notification pass writes and publishes, returned so the caller can log it.
#[derive(Debug, Default)]
pub struct NotificationOutcome {
    /// `mentionedUsersList` — the ids `IncrementMentionCount` was given and `add_mentions` carries.
    pub mentioned_users: Vec<String>,
}

fn prop_is_true(post: &Post, key: &str) -> bool {
    post.get_prop(key).and_then(|v| v.as_str()) == Some("true")
}

fn select_error(where_: &'static str, err: &dyn std::fmt::Display) -> Box<AppError> {
    tracing::error!(error = %err, "{where_} failed");
    AppError::boxed(where_, "app.select_error", None, String::new(), 500)
}

impl App {
    /// `Config().GetSanitizeOptions()` (config.go): `email` and `fullname` from the privacy
    /// settings, and the four fields an administrator always sees.
    pub fn sanitize_options(&self, as_admin: bool) -> HashMap<String, bool> {
        let mut options = HashMap::new();
        options.insert("fullname".to_owned(), self.config().show_full_name);
        options.insert("email".to_owned(), self.config().show_email_address);
        if as_admin {
            options.insert("email".to_owned(), true);
            options.insert("fullname".to_owned(), true);
            options.insert("authservice".to_owned(), true);
            options.insert("authdata".to_owned(), true);
        }
        options
    }

    /// Port of `App.handlePostEvents` (app/post.go:635) for a post this server wrote.
    ///
    /// The team is read for a channel that has one and is the **zero** team for a DM — Go builds
    /// `&model.Team{}` — so `team_id` on the `posted` event is `""` there. The three cache
    /// invalidations that follow do not exist here (this server never caches, see the
    /// vertical-slice decision), the auto-responder is a DM/GM shape the create route forwards,
    /// and outgoing webhooks are a forward condition of their own.
    #[tracing::instrument(skip_all, fields(post_id = %post.id, channel_id = %channel.id))]
    pub(crate) async fn handle_post_events(
        &self,
        post: &Post,
        user: &User,
        channel: &Channel,
        parent_post_list: Option<&PostList>,
        set_online: bool,
    ) -> AppResult<NotificationOutcome> {
        let team = if channel.team_id.is_empty() {
            Team::default()
        } else {
            self.get_team(&channel.team_id).await?
        };
        self.send_notifications(post, &team, channel, user, parent_post_list, set_online)
            .await
    }

    /// The arms of `SendNotifications` this server hands to Go, decided **before** the row is
    /// written, on the same inputs the fan-out would read. `Some(reason)` means forward.
    ///
    /// Each names text this server cannot produce: `insertGroupMentions` needs the group member
    /// lists and a translated "no users notified" notice; an out-of-channel mention sends the
    /// author a translated ephemeral post; a channel-wide mention over
    /// `MaxNotificationsPerChannel` sends three more. A message that mentions nobody outside the
    /// channel, and no group, is served — however many `@`s it carries.
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id, reason))]
    pub(crate) async fn notification_forward_reason(
        &self,
        post: &Post,
        channel: &Channel,
        team: Option<&Team>,
        parent_post_list: Option<&PostList>,
    ) -> AppResult<Option<&'static str>> {
        if channel.delete_at > 0 || post.is_notification_suppressed() {
            return Ok(None);
        }
        let profile_map = self
            .store()
            .user()
            .get_all_profiles_in_channel(&channel.id, true)
            .await
            .map_err(|err| select_error("SendNotifications", &err))?;
        let member_props = self
            .store()
            .channel()
            .get_all_channel_members_notify_props_for_channel(&channel.id, true)
            .await
            .map_err(|err| select_error("SendNotifications", &err))?;
        let groups = if self.allow_group_mentions(post).await? {
            self.get_groups_allowed_for_reference_in_channel(channel, team)
                .await?
        } else {
            BTreeMap::new()
        };
        let (mentions, _) = self
            .get_explicit_mentions_and_keywords(
                post,
                channel,
                &profile_map,
                &groups,
                &member_props,
                parent_post_list,
            )
            .await?;

        let reason = if !mentions.group_mentions.is_empty() {
            Some("a group mention needs insertGroupMentions and a translated notice")
        } else if channel.channel_type != CHANNEL_TYPE_DIRECT
            && !mentions.other_potential_mentions.is_empty()
        {
            Some("an out-of-channel mention sends a translated ephemeral notice")
        } else if profile_map.len() as i64 > self.config().max_notifications_per_channel
            && (mentions.here_mentioned || mentions.channel_mentioned || mentions.all_mentioned)
        {
            Some("a channel-wide mention over MaxNotificationsPerChannel sends a translated notice")
        } else {
            None
        };
        if let Some(reason) = reason {
            tracing::Span::current().record("reason", reason);
        }
        Ok(reason)
    }

    /// Port of `App.SendNotifications` (app/notification.go:53). See the module docs for what is
    /// and is not here.
    ///
    /// # Order of the writes and the events
    ///
    /// 1. the thread auto-follow memberships (`MaintainMembership` per participant, the poster's
    ///    with `UpdateParticipants`);
    /// 2. `IncrementMentionCount` for everyone mentioned;
    /// 3. the `posted` event, hooks attached;
    /// 4. one `thread_updated` per follower with collapsed threads on, the poster's own after
    ///    their `LastViewed` is moved to now.
    ///
    /// # Errors are Go's
    ///
    /// A failed profile or notify-props read aborts the pass (Go returns before any write). A
    /// failed membership write or mention increment is **logged and skipped** — Go warns and
    /// carries on, and the post is already committed. A failed `thread_updated` read aborts the
    /// remaining events, as Go's `return nil, err` does.
    #[tracing::instrument(skip_all, fields(post_id = %post.id, mentioned, followers))]
    pub(crate) async fn send_notifications(
        &self,
        post: &Post,
        team: &Team,
        channel: &Channel,
        sender: &User,
        parent_post_list: Option<&PostList>,
        set_online: bool,
    ) -> AppResult<NotificationOutcome> {
        // Do not send notifications in archived channels.
        if channel.delete_at > 0 {
            return Ok(NotificationOutcome::default());
        }

        let suppress_notifications = post.is_notification_suppressed();
        let is_crt_allowed = self.config().collapsed_threads != COLLAPSED_THREADS_DISABLED;

        let profile_map = self
            .store()
            .user()
            .get_all_profiles_in_channel(&channel.id, true)
            .await
            .map_err(|err| select_error("SendNotifications", &err))?;
        let member_props = self
            .store()
            .channel()
            .get_all_channel_members_notify_props_for_channel(&channel.id, true)
            .await
            .map_err(|err| select_error("SendNotifications", &err))?;

        let groups: BTreeMap<String, Group> =
            if !suppress_notifications && self.allow_group_mentions(post).await? {
                self.get_groups_allowed_for_reference_in_channel(channel, Some(team))
                    .await?
            } else {
                BTreeMap::new()
            };

        let mut followers: BTreeSet<String> = BTreeSet::new();
        if !suppress_notifications && is_crt_allowed && !post.root_id.is_empty() {
            for id in self
                .store()
                .thread()
                .get_thread_followers(&post.root_id, true)
                .await
                .map_err(|err| select_error("SendNotifications", &err))?
            {
                followers.insert(id);
            }
        }

        let mut mentioned_users_list: Vec<String> = Vec::new();
        let mut notifications_for_crt = CrtNotifiers::default();
        let mut mentions = MentionResults::default();
        let mut new_participants: BTreeSet<String> = BTreeSet::new();
        let mut participant_memberships: BTreeMap<String, ThreadMembership> = BTreeMap::new();

        if !suppress_notifications {
            let (found, keywords) = self
                .get_explicit_mentions_and_keywords(
                    post,
                    channel,
                    &profile_map,
                    &groups,
                    &member_props,
                    parent_post_list,
                )
                .await?;
            mentions = found;

            if channel.channel_type != CHANNEL_TYPE_DIRECT {
                // `insertGroupMentions` and `sendOutOfChannelMentions` — both are forward
                // conditions decided before the write, so reaching either here is a bug in
                // that gate rather than a branch to take.
                if !mentions.group_mentions.is_empty() {
                    tracing::warn!(post_id = %post.id, "a group mention reached the fan-out; the pre-write gate should have forwarded it");
                }
                if !mentions.other_potential_mentions.is_empty() {
                    tracing::warn!(post_id = %post.id, potential = ?mentions.other_potential_mentions, "an out-of-channel mention reached the fan-out; the pre-write gate should have forwarded it");
                }
                // `allActivityPushUserIds` feeds push only — [D-402].
            }

            let thread_auto_follow = self.config().thread_auto_follow;
            if thread_auto_follow && !post.root_id.is_empty() {
                let mut thread_participants: BTreeSet<String> = BTreeSet::new();
                thread_participants.insert(post.user_id.clone());

                let auto_follow_off = |id: &str| -> bool {
                    profile_map.get(id).is_some_and(|profile| {
                        user_prop(
                            profile.notify_props.as_ref(),
                            CHANNEL_MENTION_AUTO_FOLLOW_THREADS_PROP,
                        ) == "false"
                    })
                };

                if let Some(parent) = parent_post_list {
                    if let Some(root_post) = root_of(parent) {
                        if !prop_is_true(root_post, POST_PROPS_FROM_WEBHOOK)
                            && profile_map.contains_key(&root_post.user_id)
                        {
                            thread_participants.insert(root_post.user_id.clone());
                        }
                        if channel.channel_type != CHANNEL_TYPE_DIRECT {
                            let root_mentions =
                                get_explicit_mentions(root_post, &keywords, MM_BLOCKS_ENABLED);
                            for (id, mention_type) in &root_mentions.mentions {
                                if *mention_type == MentionType::ChannelMention
                                    && auto_follow_off(id)
                                {
                                    continue;
                                }
                                thread_participants.insert(id.clone());
                            }
                        }
                    }
                }
                for (id, mention_type) in &mentions.mentions {
                    if *mention_type == MentionType::ChannelMention && auto_follow_off(id) {
                        continue;
                    }
                    thread_participants.insert(id.clone());
                }
                if channel.channel_type != CHANNEL_TYPE_DIRECT {
                    for (id, props) in &member_props {
                        if !followers.contains(id)
                            && props.get(CHANNEL_AUTO_FOLLOW_THREADS).map(String::as_str)
                                == Some(CHANNEL_AUTO_FOLLOW_THREADS_ON)
                        {
                            thread_participants.insert(id.clone());
                        }
                    }
                }

                // Go fans these out eight at a time; each is an independent row, so the order
                // here is the set's and the result is the same.
                for user_id in &thread_participants {
                    let (mention_type, mut increment_mentions) =
                        match mentions.mentions.get(user_id) {
                            Some(mention_type) => (*mention_type, true),
                            None => (MentionType::NoMention, false),
                        };

                    // If the user was not explicitly mentioned, check if they explicitly
                    // unfollowed the thread.
                    if !increment_mentions {
                        match self
                            .store()
                            .thread()
                            .get_membership_for_user(user_id, &post.root_id)
                            .await
                        {
                            Ok(membership) => {
                                if !membership.following {
                                    continue;
                                }
                            }
                            Err(err) if err.is_not_found() => {}
                            Err(err) => {
                                tracing::warn!(error = %err, post_id = %post.id, channel_id = %post.channel_id, "Failed to update thread autofollow from mention");
                                continue;
                            }
                        }
                    }

                    let mut update_following = thread_auto_follow;
                    if mention_type == MentionType::ThreadMention
                        || mention_type == MentionType::CommentMention
                    {
                        increment_mentions = false;
                        update_following = false;
                    }
                    let opts = ThreadMembershipOpts {
                        following: true,
                        increment_mentions,
                        update_following,
                        update_viewed_timestamp: false,
                        update_participants: user_id == &post.user_id,
                    };
                    let thread_membership = match self
                        .store()
                        .thread()
                        .maintain_membership(user_id, &post.root_id, opts)
                        .await
                    {
                        Ok(membership) => membership,
                        Err(err) => {
                            tracing::warn!(error = %err, post_id = %post.id, channel_id = %post.channel_id, "Failed to update thread autofollow from mention");
                            continue;
                        }
                    };

                    if !followers.contains(user_id) && thread_membership.following {
                        followers.insert(user_id.clone());
                        new_participants.insert(user_id.clone());
                    }
                    participant_memberships.insert(user_id.clone(), thread_membership);
                }
            }

            mentioned_users_list = mentions.mentions.keys().cloned().collect();

            if let Err(err) = self
                .store()
                .channel()
                .increment_mention_count(
                    &post.channel_id,
                    &mentioned_users_list,
                    post.root_id.is_empty(),
                    post.is_urgent(),
                )
                .await
            {
                tracing::warn!(error = %err, post_id = %post.id, channel_id = %post.channel_id, "Failed to update mention count");
            }

            if is_crt_allowed && !post.root_id.is_empty() {
                for uid in &followers {
                    let Some(profile) = profile_map.get(uid) else {
                        continue;
                    };
                    if !self.is_crt_enabled_for_user(uid).await {
                        continue;
                    }
                    if !prop_is_true(post, POST_PROPS_FROM_WEBHOOK) && uid == &post.user_id {
                        continue;
                    }
                    let empty = StringMap::new();
                    notifications_for_crt.add_follower_to_notify(
                        profile,
                        &mentions,
                        member_props.get(uid).unwrap_or(&empty),
                        channel,
                    );
                }
            }
        }
        tracing::Span::current().record("mentioned", mentioned_users_list.len());
        tracing::Span::current().record("followers", followers.len());

        let notification = PostNotification {
            post,
            channel,
            profile_map: &profile_map,
            sender,
        };

        // Email and push — [D-402]. The over-limit channel-wide notice is a forward condition.

        let mut message =
            WebSocketEvent::new(WEBSOCKET_EVENT_POSTED, "", &post.channel_id, "", None, "");
        message.add(
            "channel_type",
            serde_json::Value::String(channel.channel_type.clone()),
        );
        message.add(
            "channel_display_name",
            serde_json::Value::String(notification.get_channel_name(SHOW_USERNAME, "")),
        );
        message.add(
            "channel_name",
            serde_json::Value::String(channel.name.clone()),
        );
        message.add(
            "sender_name",
            serde_json::Value::String(
                notification
                    .get_sender_name(SHOW_USERNAME, self.config().enable_post_username_override),
            ),
        );
        message.add("team_id", serde_json::Value::String(team.id.clone()));
        message.add("set_online", serde_json::Value::Bool(set_online));
        // `otherFile`/`image` need file ids, which the create route refuses.

        if !mentioned_users_list.is_empty() {
            let mut args = StringInterface::new();
            args.insert(
                "mentions".to_owned(),
                serde_json::Value::from(mentioned_users_list.clone()),
            );
            if let Some(broadcast) = message.broadcast.as_mut() {
                broadcast.add_hook(BROADCAST_ADD_MENTIONS, args);
            }
        }
        if !notifications_for_crt.desktop.is_empty() {
            let mut args = StringInterface::new();
            args.insert(
                "followers".to_owned(),
                serde_json::Value::from(notifications_for_crt.desktop.clone()),
            );
            if let Some(broadcast) = message.broadcast.as_mut() {
                broadcast.add_hook(BROADCAST_ADD_FOLLOWERS, args);
            }
        }

        // Collect user IDs of whom we want to acknowledge the websocket event.
        let mut users_to_ack: Vec<String> = Vec::new();
        for (id, profile) in &profile_map {
            let user_level = user_prop(profile.notify_props.as_ref(), DESKTOP_NOTIFY_PROP);
            let channel_level = member_props
                .get(id)
                .and_then(|props| props.get(DESKTOP_NOTIFY_PROP))
                .map_or("", String::as_str);
            if should_ack_websocket_notification(&channel.channel_type, user_level, channel_level) {
                users_to_ack.push(id.clone());
            }
        }
        {
            let mut args = StringInterface::new();
            args.insert(
                "posted_user_id".to_owned(),
                serde_json::Value::String(post.user_id.clone()),
            );
            args.insert(
                "channel_type".to_owned(),
                serde_json::Value::String(channel.channel_type.clone()),
            );
            args.insert("users".to_owned(), serde_json::Value::from(users_to_ack));
            if let Some(broadcast) = message.broadcast.as_mut() {
                broadcast.add_hook(BROADCAST_POSTED_ACK, args);
            }
        }

        self.publish_posted_event_with_hooks(post, message).await?;

        // If this is a reply in a thread, notify participants.
        if !suppress_notifications && is_crt_allowed && !post.root_id.is_empty() {
            for uid in &followers {
                if !profile_map.contains_key(uid) {
                    // A bot, a deactivated user, or someone who left the channel (MM-36769).
                    continue;
                }
                if !self.is_crt_enabled_for_user(uid).await {
                    continue;
                }
                let mut message = WebSocketEvent::new(
                    WEBSOCKET_EVENT_THREAD_UPDATED,
                    &team.id,
                    "",
                    uid,
                    None,
                    "",
                );
                let thread_membership = match participant_memberships.get(uid) {
                    Some(membership) => membership.clone(),
                    None => self
                        .store()
                        .thread()
                        .get_membership_for_user(uid, &post.root_id)
                        .await
                        .map_err(|err| {
                            tracing::error!(error = %err, user_id = %uid, thread_id = %post.root_id, "Missing thread membership for participant in notifications");
                            select_error("SendNotifications", &err)
                        })?,
                };
                let mut user_thread = self.get_thread_for_user(&thread_membership, true).await?;

                let mut previous_unread_mentions = 0_i64;
                let mut previous_unread_replies = 0_i64;
                // If it's not a newly followed thread, calculate previous unread values.
                if !new_participants.contains(uid) {
                    previous_unread_mentions = user_thread.unread_mentions;
                    previous_unread_replies = (user_thread.unread_replies - 1).max(0);
                    if mentions.is_user_mentioned(uid) {
                        previous_unread_mentions = (user_thread.unread_mentions - 1).max(0);
                    }
                }

                // Set LastViewed to now for the commenter.
                if uid == &post.user_id {
                    self.store()
                        .thread()
                        .maintain_membership(
                            uid,
                            &post.root_id,
                            ThreadMembershipOpts {
                                following: false,
                                increment_mentions: false,
                                update_following: false,
                                update_viewed_timestamp: true,
                                update_participants: false,
                            },
                        )
                        .await
                        .map_err(|err| select_error("SendNotifications", &err))?;
                    user_thread.unread_mentions = 0;
                    user_thread.unread_replies = 0;
                }

                // `sanitizeThreadResponse`: the participants as a non-admin sees them; the post's
                // props and integrations were already stripped by `get_thread_for_user`.
                let options = self.sanitize_options(false);
                for participant in user_thread.participants.iter_mut().flatten() {
                    participant.sanitize_profile(&options, false);
                }
                if let Some(thread_post) = user_thread.post.take() {
                    let (sanitized, _is_member_for_preview) = self
                        .sanitize_post_metadata_for_user(*thread_post, uid)
                        .await
                        .map_err(|err| match err {
                            crate::post::PrepareError::App(app_error) => app_error,
                            other => select_error("SendNotifications", &other),
                        })?;
                    user_thread.post = Some(Box::new(sanitized));
                }

                let payload = go_json_marshal(&user_thread).map_err(|err| {
                    tracing::warn!(error = %err, "Failed to encode thread to JSON");
                    select_error("SendNotifications", &err)
                })?;
                message.add("thread", serde_json::Value::String(payload));
                message.add(
                    "previous_unread_mentions",
                    serde_json::Value::from(previous_unread_mentions),
                );
                message.add(
                    "previous_unread_replies",
                    serde_json::Value::from(previous_unread_replies),
                );
                self.publish(message).await;
            }
        }

        Ok(NotificationOutcome {
            mentioned_users: mentioned_users_list,
        })
    }

    /// Port of `App.publishWebsocketEventForPost` (app/post.go) for the shapes the create route
    /// serves: the post is serialised **once**, after the permalink metadata and the
    /// `channel_mentions` prop would have been removed — neither exists on a post that reaches
    /// here, since links and `~` mentions are forwarded — and the two hooks they would attach
    /// are therefore not attached.
    async fn publish_posted_event_with_hooks(
        &self,
        post: &Post,
        mut message: WebSocketEvent,
    ) -> AppResult<()> {
        let post_json = post.to_json().map_err(|err| {
            tracing::error!(error = %err, post_id = %post.id, "Error in marshalling post to JSON");
            AppError::boxed(
                "publishWebsocketEventForPost",
                "app.post.marshal.app_error",
                None,
                String::new(),
                500,
            )
        })?;
        message.add("post", serde_json::Value::String(post_json));
        self.publish(message).await;
        Ok(())
    }

    /// Port of `App.getExplicitMentionsAndKeywords` (notification.go:1066).
    ///
    /// # A DM mentions the other side, and nothing else
    ///
    /// No keywords at all: each of the two users named in the channel is mentioned unless they
    /// are the poster (a webhook post mentions both). The empty keyword table it returns is what
    /// the thread-participant pass then uses for the root post.
    ///
    /// # Everywhere else, four additions and one removal
    ///
    /// The keyword mentions; every member of a **group** message (`GMMention`, the lowest
    /// priority, so a keyword hit on the same user wins); the user an add-to-channel post names;
    /// and, for a reply, the thread's earlier authors whose `comments` notify prop asks for it —
    /// `any`, or `root` for the root's author only — as long as collapsed threads are **off** for
    /// them and the root was not posted by an OAuth bot. Then the poster is removed, unless the
    /// post is a webhook's.
    pub(crate) async fn get_explicit_mentions_and_keywords(
        &self,
        post: &Post,
        channel: &Channel,
        profile_map: &BTreeMap<String, User>,
        groups: &BTreeMap<String, Group>,
        member_props: &BTreeMap<String, StringMap>,
        parent_post_list: Option<&PostList>,
    ) -> AppResult<(MentionResults, MentionKeywords)> {
        let mut mentions = MentionResults::default();
        let mut keywords = MentionKeywords::new();

        if channel.channel_type == CHANNEL_TYPE_DIRECT {
            let is_webhook = prop_is_true(post, POST_PROPS_FROM_WEBHOOK);
            // A bot can post in a DM where it doesn't belong to, so both named users are
            // candidates rather than "the other one".
            let (user1, user2) = channel.get_both_users_for_dm();
            if post.user_id != user1 || is_webhook {
                if profile_map.contains_key(user1) {
                    mentions.add_mention(user1, MentionType::DmMention);
                } else {
                    tracing::debug!(user_id = user1, channel_id = %channel.id, "missing profile: DM user not in profiles");
                }
            }
            if !user2.is_empty() && (post.user_id != user2 || is_webhook) {
                if profile_map.contains_key(user2) {
                    mentions.add_mention(user2, MentionType::DmMention);
                } else {
                    tracing::debug!(user_id = user2, channel_id = %channel.id, "missing profile: DM user not in profiles");
                }
            }
            return Ok((mentions, keywords));
        }

        let allow_channel_mentions = self.allow_channel_mentions(post, profile_map.len()).await;
        keywords = self
            .get_mention_keywords_in_channel(
                profile_map,
                allow_channel_mentions,
                member_props,
                groups,
            )
            .await?;

        mentions = get_explicit_mentions(post, &keywords, MM_BLOCKS_ENABLED);

        // Add a GM mention to all members of a GM channel.
        if channel.channel_type == CHANNEL_TYPE_GROUP {
            for id in member_props.keys() {
                if profile_map.contains_key(id) {
                    mentions.add_mention(id, MentionType::GmMention);
                } else {
                    tracing::debug!(user_id = %id, channel_id = %channel.id, "missing profile: GM user not in profiles");
                }
            }
        }

        // Add an implicit mention when a user is added to a channel even if the user has set
        // 'username mentions' to false in account settings.
        if post.post_type == POST_TYPE_ADD_TO_CHANNEL {
            if let Some(added) = post
                .get_prop(POST_PROPS_ADDED_USER_ID)
                .and_then(|v| v.as_str())
            {
                if profile_map.contains_key(added) {
                    mentions.add_mention(added, MentionType::KeywordMention);
                } else {
                    tracing::debug!(user_id = added, channel_id = %channel.id, "missing profile: user added to channel not in profiles");
                }
            }
        }

        // Get users that have comment thread mentions enabled.
        if !post.root_id.is_empty() {
            if let Some(parent) = parent_post_list {
                let root_id = parent
                    .order
                    .as_ref()
                    .and_then(|order| order.first())
                    .map(String::as_str)
                    .unwrap_or("");
                for thread_post in parent.posts.iter().flat_map(|posts| posts.values()) {
                    let Some(profile) = profile_map.get(&thread_post.user_id) else {
                        continue;
                    };
                    let is_root = thread_post.id == root_id;
                    // If this is the root post and it was posted by an OAuth bot, don't notify
                    // the user.
                    if is_root && thread_post.is_from_oauth_bot() {
                        continue;
                    }
                    if self.is_crt_enabled_for_user(&profile.id).await {
                        continue;
                    }
                    let comments = user_prop(profile.notify_props.as_ref(), COMMENTS_NOTIFY_PROP);
                    if comments == COMMENTS_NOTIFY_ANY
                        || (comments == COMMENTS_NOTIFY_ROOT && is_root)
                    {
                        let mention_type = if is_root {
                            MentionType::CommentMention
                        } else {
                            MentionType::ThreadMention
                        };
                        mentions.add_mention(&thread_post.user_id, mention_type);
                    }
                }
            }
        }

        // Prevent the user from mentioning themselves.
        if !prop_is_true(post, POST_PROPS_FROM_WEBHOOK) {
            mentions.remove_mention(&post.user_id);
        }

        Ok((mentions, keywords))
    }

    /// Port of `App.allowChannelMentions` (notification.go:1464): the `use_channel_mentions`
    /// permission, not a header or purpose change, and fewer members than
    /// `MaxNotificationsPerChannel` — **`>=` refuses**, so a channel exactly at the limit has no
    /// `@channel`.
    async fn allow_channel_mentions(&self, post: &Post, num_profiles: usize) -> bool {
        let (ok, _) = self
            .has_permission_to_channel(
                &post.user_id,
                &post.channel_id,
                &PERMISSION_USE_CHANNEL_MENTIONS,
            )
            .await;
        if !ok {
            return false;
        }
        if post.post_type == POST_TYPE_HEADER_CHANGE || post.post_type == POST_TYPE_PURPOSE_CHANGE {
            return false;
        }
        if num_profiles as i64 >= self.config().max_notifications_per_channel {
            return false;
        }
        true
    }

    /// Port of `App.allowGroupMentions` (notification.go:1481): a professional licence first,
    /// then `use_group_mentions`, then the post type.
    pub(crate) async fn allow_group_mentions(&self, post: &Post) -> AppResult<bool> {
        let license = self.license().await?;
        if !minimum_professional_license(license.as_deref()) {
            return Ok(false);
        }
        let (ok, _) = self
            .has_permission_to_channel(
                &post.user_id,
                &post.channel_id,
                &PERMISSION_USE_GROUP_MENTIONS,
            )
            .await;
        if !ok {
            return Ok(false);
        }
        if post.post_type == POST_TYPE_HEADER_CHANGE || post.post_type == POST_TYPE_PURPOSE_CHANGE {
            return Ok(false);
        }
        Ok(true)
    }

    /// Port of `App.getMentionKeywordsInChannel` (notification.go:1548): every member's keywords
    /// with their channel notify props and their status, then the groups.
    ///
    /// The status is what decides `@here` per member. Go's `GetStatusFromCache` is nil on a
    /// cache miss; this reads the rows — see the module docs.
    async fn get_mention_keywords_in_channel(
        &self,
        profiles: &BTreeMap<String, User>,
        allow_channel_mentions: bool,
        member_props: &BTreeMap<String, StringMap>,
        groups: &BTreeMap<String, Group>,
    ) -> AppResult<MentionKeywords> {
        let ids: Vec<String> = profiles.keys().cloned().collect();
        let statuses: BTreeMap<String, Status> = if ids.is_empty() {
            BTreeMap::new()
        } else {
            self.store()
                .status()
                .get_by_ids(&ids)
                .await
                .map_err(|err| select_error("getMentionKeywordsInChannel", &err))?
                .into_iter()
                .map(|status| (status.user_id.clone(), status))
                .collect()
        };

        let empty = StringMap::new();
        let mut keywords = MentionKeywords::new();
        for profile in profiles.values() {
            keywords.add_user(
                profile,
                member_props.get(&profile.id).unwrap_or(&empty),
                statuses.get(&profile.id),
                allow_channel_mentions,
            );
        }
        keywords.add_groups_map(groups.values());
        Ok(keywords)
    }
}

/// `parentPostList.Posts[parentPostList.Order[0]]` — the root of the thread the reply is in.
fn root_of(parent: &PostList) -> Option<&Post> {
    let root_id = parent.order.as_ref()?.first()?;
    parent.posts.as_ref()?.get(root_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_with(props: &[(&str, &str)]) -> User {
        User {
            id: "u".to_owned(),
            notify_props: Some(
                props
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            ),
            ..Default::default()
        }
    }

    fn props(pairs: &[(&str, &str)]) -> StringMap {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    // ---- shouldUserNotifyCRT ---------------------------------------------------------------

    #[test]
    fn user_crt_desktop_needs_not_none_and_a_reason() {
        let mention_only = user_with(&[
            ("desktop", "mention"),
            ("push", "mention"),
            ("email", "true"),
        ]);
        assert_eq!(
            should_user_notify_crt(&mention_only, true),
            (true, true, true)
        );
        assert_eq!(
            should_user_notify_crt(&mention_only, false),
            (false, false, false)
        );

        let all = user_with(&[("desktop", "all"), ("push", "all"), ("email", "false")]);
        assert_eq!(should_user_notify_crt(&all, false), (true, true, false));

        let threads_all = user_with(&[
            ("desktop", "mention"),
            ("desktop_threads", "all"),
            ("push", "mention"),
            ("push_threads", "all"),
            ("email", "true"),
            ("email_threads", "all"),
        ]);
        assert_eq!(
            should_user_notify_crt(&threads_all, false),
            (true, true, true)
        );

        let none = user_with(&[("desktop", "none"), ("push", "none"), ("email", "true")]);
        assert_eq!(
            should_user_notify_crt(&none, true),
            (false, false, true),
            "none beats a mention; email has no none"
        );
    }

    #[test]
    fn a_user_without_notify_props_is_never_notified_by_crt() {
        let bare = User::default();
        // `desktop != none` holds for "", so a mention still notifies desktop and push.
        assert_eq!(should_user_notify_crt(&bare, true), (true, true, false));
        assert_eq!(should_user_notify_crt(&bare, false), (false, false, false));
    }

    // ---- shouldChannelMemberNotifyCRT -------------------------------------------------------

    #[test]
    fn member_crt_default_and_none_never_notify() {
        // An **empty** level is neither `default` nor `none`, so it does notify on a mention —
        // Go's two inequalities, not a "set" test.
        let member = props(&[("desktop", ""), ("push", "")]);
        assert_eq!(
            should_channel_member_notify_crt(None, &member, true),
            (true, true)
        );
        for level in ["default", "none"] {
            let member = props(&[("desktop", level), ("push", level)]);
            assert_eq!(
                should_channel_member_notify_crt(None, &member, true),
                (false, false),
                "{level:?}"
            );
        }
    }

    #[test]
    fn member_crt_desktop_threads_all_is_vetoed_by_the_users_mention_setting() {
        let member = props(&[("desktop", "mention"), ("desktop_threads", "all")]);
        assert_eq!(
            should_channel_member_notify_crt(None, &member, false),
            (true, false)
        );
        let user = props(&[("desktop_threads", "mention")]);
        assert_eq!(
            should_channel_member_notify_crt(Some(&user), &member, false),
            (false, false),
            "the user's own desktop_threads=mention wins"
        );
        // The push arm has no such veto.
        let member = props(&[("push", "mention"), ("push_threads", "all")]);
        let user = props(&[("push_threads", "mention")]);
        assert_eq!(
            should_channel_member_notify_crt(Some(&user), &member, false),
            (false, true)
        );
    }

    // ---- addFollowerToNotify ---------------------------------------------------------------

    #[test]
    fn follower_notify_uses_the_user_props_only_under_default_channel_props() {
        let channel = Channel::default();
        let mentions = MentionResults::default();
        let user = user_with(&[
            ("desktop", "all"),
            ("push", "all"),
            ("email", "true"),
            ("email_threads", "all"),
        ]);

        // Channel props at default: the user's `all` applies.
        let mut crt = CrtNotifiers::default();
        crt.add_follower_to_notify(
            &user,
            &mentions,
            &props(&[("desktop", "default"), ("push", "default")]),
            &channel,
        );
        assert_eq!(
            crt,
            CrtNotifiers {
                desktop: vec!["u".into()],
                email: vec!["u".into()],
                push: vec!["u".into()]
            }
        );

        // Channel props say `none`: the user's `all` is not consulted, and the member rule
        // refuses `none` — so nothing but email.
        let mut crt = CrtNotifiers::default();
        crt.add_follower_to_notify(
            &user,
            &mentions,
            &props(&[("desktop", "none"), ("push", "none")]),
            &channel,
        );
        assert_eq!(
            crt,
            CrtNotifiers {
                desktop: vec![],
                email: vec!["u".into()],
                push: vec![]
            }
        );

        // Channel props say `all` for a user set to `mention`: the member rule admits.
        let quiet = user_with(&[
            ("desktop", "mention"),
            ("push", "mention"),
            ("email", "false"),
        ]);
        let mut crt = CrtNotifiers::default();
        crt.add_follower_to_notify(
            &quiet,
            &mentions,
            &props(&[("desktop", "all"), ("push", "all")]),
            &channel,
        );
        assert_eq!(
            crt,
            CrtNotifiers {
                desktop: vec!["u".into()],
                email: vec![],
                push: vec!["u".into()]
            }
        );
    }

    // ---- shouldAckWebsocketNotification -----------------------------------------------------

    #[test]
    fn ack_rule_table() {
        assert!(should_ack_websocket_notification("O", "mention", "all"));
        assert!(should_ack_websocket_notification("O", "all", "default"));
        assert!(!should_ack_websocket_notification(
            "O", "mention", "default"
        ));
        assert!(!should_ack_websocket_notification("O", "all", "mention"));
        assert!(!should_ack_websocket_notification("O", "all", "none"));
        // Group messages: default+mention, or the channel at mention, ack.
        assert!(should_ack_websocket_notification("G", "mention", "default"));
        assert!(should_ack_websocket_notification("G", "all", "mention"));
        assert!(!should_ack_websocket_notification("G", "none", "default"));
        assert!(
            !should_ack_websocket_notification("D", "mention", "default"),
            "a DM is acked by the hook's own rule, not this one"
        );
    }

    // ---- PostNotification --------------------------------------------------------------------

    #[test]
    fn channel_name_by_channel_type() {
        let sender = User {
            id: "s".into(),
            username: "sender".into(),
            ..Default::default()
        };
        let mut profiles = BTreeMap::new();
        for (id, name) in [("a", "zed"), ("b", "amy"), ("s", "sender")] {
            profiles.insert(
                id.to_owned(),
                User {
                    id: id.into(),
                    username: name.into(),
                    ..Default::default()
                },
            );
        }
        let post = Post::default();
        let mut channel = Channel {
            display_name: "Town".into(),
            ..Default::default()
        };

        channel.channel_type = "O".into();
        let n = PostNotification {
            post: &post,
            channel: &channel,
            profile_map: &profiles,
            sender: &sender,
        };
        assert_eq!(n.get_channel_name(SHOW_USERNAME, ""), "Town");

        channel.channel_type = "D".into();
        let n = PostNotification {
            post: &post,
            channel: &channel,
            profile_map: &profiles,
            sender: &sender,
        };
        assert_eq!(n.get_channel_name(SHOW_USERNAME, ""), "@sender");

        channel.channel_type = "G".into();
        let n = PostNotification {
            post: &post,
            channel: &channel,
            profile_map: &profiles,
            sender: &sender,
        };
        assert_eq!(
            n.get_channel_name(SHOW_USERNAME, ""),
            "amy, sender, zed",
            "sorted"
        );
        assert_eq!(
            n.get_channel_name(SHOW_USERNAME, "s"),
            "amy, zed",
            "minus the excluded id"
        );
    }

    #[test]
    fn sender_name_honours_the_webhook_override_only_when_allowed_and_flagged() {
        let sender = User {
            id: "s".into(),
            username: "sender".into(),
            ..Default::default()
        };
        let profiles = BTreeMap::new();
        let channel = Channel {
            channel_type: "O".into(),
            ..Default::default()
        };
        let mut post = Post::default();
        post.add_prop(
            "override_username",
            serde_json::Value::String("hook".into()),
        );

        let n = PostNotification {
            post: &post,
            channel: &channel,
            profile_map: &profiles,
            sender: &sender,
        };
        assert_eq!(
            n.get_sender_name(SHOW_USERNAME, true),
            "@sender",
            "no from_webhook"
        );
        post.add_prop("from_webhook", serde_json::Value::String("true".into()));
        let n = PostNotification {
            post: &post,
            channel: &channel,
            profile_map: &profiles,
            sender: &sender,
        };
        assert_eq!(n.get_sender_name(SHOW_USERNAME, true), "hook");
        assert_eq!(
            n.get_sender_name(SHOW_USERNAME, false),
            "@sender",
            "overrides disabled"
        );
        let dm = Channel {
            channel_type: "D".into(),
            ..Default::default()
        };
        let n = PostNotification {
            post: &post,
            channel: &dm,
            profile_map: &profiles,
            sender: &sender,
        };
        assert_eq!(
            n.get_sender_name(SHOW_USERNAME, true),
            "@sender",
            "never in a DM"
        );
    }

    #[test]
    fn root_of_reads_order_zero() {
        let mut list = PostList::default();
        assert!(root_of(&list).is_none());
        let root = Post {
            id: "r".into(),
            ..Default::default()
        };
        let reply = Post {
            id: "x".into(),
            root_id: "r".into(),
            ..Default::default()
        };
        let mut posts = BTreeMap::new();
        posts.insert("x".to_owned(), reply);
        posts.insert("r".to_owned(), root);
        list.posts = Some(posts.into_iter().collect());
        list.order = Some(vec!["r".to_owned(), "x".to_owned()]);
        assert_eq!(root_of(&list).map(|p| p.id.as_str()), Some("r"));
    }
}
