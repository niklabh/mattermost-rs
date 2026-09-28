//! Port of `app/post_persistent_notification.go` — persistent notifications: the
//! `PersistentNotifications` rows a priority post with `persistent_notifications: true` leaves
//! behind, the job that re-notifies its mentioned users from them, and the three ways a row ends.
//!
//! # One walk, three callers
//!
//! [`App::for_each_persistent_notification_post`] is Go's `forEachPersistentNotificationPost`:
//! it resolves each post's channel and team, builds per-channel `@username` keywords from the
//! channel's non-bot members (the channel's referenceable groups too, outside a DM), computes the
//! post's mentions, and hands each post to a visitor. Go passes a closure; the three closures are
//! the variants of [`Visit`]:
//!
//! | caller | visitor | Go |
//! |---|---|---|
//! | `CreatePost`, before the save | the recipient count, 1 ..= `PersistentNotificationMaxRecipients` | post.go:230 |
//! | `ResolvePersistentNotification` | whether the resolving user is mentioned | :57 |
//! | the job, `SendPersistentNotifications` | push and the `persistent_notification_triggered` event | :316 |
//!
//! A post that is notification-suppressed, in a channel that is gone (or archived: the channel
//! read excludes deleted ones), or in a team that is gone is not visited but **retired** after the
//! walk — its row soft-deleted — which is how a stale row stops being picked up.
//!
//! # A DM mentions only the other side
//!
//! No keywords at all in a direct channel: the one mention is the other user, as a `DmMention`,
//! and only when that user is still in the channel's profile map.

use std::collections::{BTreeMap, HashMap};

use mm_model::channel::{CHANNEL_TYPE_DIRECT, CHANNEL_TYPE_GROUP, Channel};
use mm_model::group::Group;
use mm_model::post::Post;
use mm_model::status::{STATUS_DND, STATUS_OFFLINE, STATUS_OUT_OF_OFFICE, Status};
use mm_model::team::Team;
use mm_model::user::external::SHOW_USERNAME;
use mm_model::user::{DESKTOP_NOTIFY_PROP, USER_NOTIFY_NONE, User};
use mm_model::utils::{AppError, StringMap, get_millis};
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_PERSISTENT_NOTIFICATION_TRIGGERED, WebSocketEvent,
};
use mm_store::group_lookup_store::GroupLookupStore;
use mm_store::post_store::PostStore;
use mm_store::user_store::UserStore;
use mm_store::{ChannelStore, FileInfoStore, TeamStore};

use crate::App;
use crate::mention::{MentionKeywords, MentionResults, MentionType, get_explicit_mentions};
use crate::notification::PostNotification;
use crate::post::{PrepareError, PreparePostForClientOpts};

/// `ServiceSettings.PersistentNotificationIntervalMinutes` default (config.go:1005).
const DEFAULT_INTERVAL_MINUTES: i64 = 5;
/// `ServiceSettings.PersistentNotificationMaxCount` default (config.go:1009).
const DEFAULT_MAX_COUNT: i64 = 6;
/// `ServiceSettings.PersistentNotificationMaxRecipients` default (config.go:1013).
const DEFAULT_MAX_RECIPIENTS: i64 = 5;
/// `SendPersistentNotifications`' page size.
const PER_PAGE: i64 = 500;

/// Go's plain `error` out of the walk and the job — `errors.Wrap` text, kept because it is what a
/// failed job writes into `Data["error"]`.
#[derive(Debug, thiserror::Error)]
pub enum PersistentNotificationError {
    /// `errors.Wrap(err, context)`, rendered as Go renders it: `context: cause`.
    #[error("{context}: {source}")]
    Wrapped {
        context: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// `errors.Errorf`, a message with no cause.
    #[error("{0}")]
    Message(String),
    /// An `*model.AppError` a visitor returned as the walk's error.
    #[error("{0}")]
    App(Box<AppError>),
    /// A post whose client shape this server cannot produce. Go would send; this server cannot
    /// say what it would send, so the run fails instead of sending something else.
    #[error("persistent notification is not reproducible here: {0}")]
    Unreproducible(&'static str),
}

impl PersistentNotificationError {
    fn wrap(
        context: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Wrapped {
            context: context.into(),
            source: Box::new(source),
        }
    }
}

/// The three closures `forEachPersistentNotificationPost` is called with.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Visit<'a> {
    /// `CreatePost`'s recipient check, against `PersistentNotificationMaxRecipients`.
    ValidateRecipients { max_recipients: i64 },
    /// `ResolvePersistentNotification`'s: is this user mentioned?
    IsMentioned {
        user_id: &'a str,
        mentioned: &'a std::sync::atomic::AtomicBool,
    },
    /// The job's `sendPersistentNotifications`.
    Send,
}

/// The persistent-notification settings, read from the whole `model.Config` — they are not in
/// the narrow [`crate::config::Config`] — with Go's defaults for an absent key.
pub(crate) struct PersistentSettings {
    pub interval_minutes: i64,
    pub max_count: i64,
    pub max_recipients: i64,
}

impl App {
    /// Port of `App.IsPersistentNotificationsEnabled` (post_persistent_notification.go:430) —
    /// re-exported under the name the job and create paths use; see
    /// [`App::is_persistent_notifications_enabled`].
    pub(crate) async fn persistent_settings(&self) -> PersistentSettings {
        let service = crate::config::load_model_config(self.store().config())
            .await
            .map(|config| config.service_settings)
            .ok();
        let read = |value: Option<i64>, default: i64| value.unwrap_or(default);
        PersistentSettings {
            interval_minutes: read(
                service
                    .as_ref()
                    .and_then(|s| s.persistent_notification_interval_minutes),
                DEFAULT_INTERVAL_MINUTES,
            ),
            max_count: read(
                service
                    .as_ref()
                    .and_then(|s| s.persistent_notification_max_count),
                DEFAULT_MAX_COUNT,
            ),
            max_recipients: read(
                service
                    .as_ref()
                    .and_then(|s| s.persistent_notification_max_recipients),
                DEFAULT_MAX_RECIPIENTS,
            ),
        }
    }

    /// Port of `App.ResolvePersistentNotification` (post_persistent_notification.go:20): a
    /// reaction, a reply or an acknowledgement by a user the post **mentions** stops its
    /// notifications. The author's own actions never do; with guests excluded from the feature a
    /// guest's do not either. Every failure after the row lookup is
    /// `app.post_priority.delete_persistent_notification_post.app_error`.
    pub async fn resolve_persistent_notification(
        &self,
        post: &Post,
        logged_in_user_id: &str,
    ) -> Result<(), Box<AppError>> {
        let delete_error = |err: &dyn std::fmt::Display| {
            tracing::error!(error = %err, "resolving the persistent notification failed");
            AppError::boxed(
                "ResolvePersistentNotification",
                "app.post_priority.delete_persistent_notification_post.app_error",
                None,
                String::new(),
                500,
            )
        };

        // Ignore the post owner's actions to their own post.
        if logged_in_user_id == post.user_id {
            return Ok(());
        }
        if !self.is_persistent_notifications_enabled() {
            return Ok(());
        }
        let live = self
            .store()
            .post()
            .has_persistent_notification(&post.id)
            .await
            .map_err(|err| delete_error(&err))?;
        if !live {
            // Either the notification post is already deleted or was never a notification post.
            return Ok(());
        }

        if !self.config().allow_persistent_notifications_for_guests {
            let user = self
                .store()
                .user()
                .get(logged_in_user_id)
                .await
                .map_err(|err| {
                    if err.is_not_found() {
                        AppError::boxed(
                            "ResolvePersistentNotification",
                            "app.user.missing_account.const",
                            None,
                            String::new(),
                            404,
                        )
                    } else {
                        tracing::error!(error = %err, "user lookup failed");
                        AppError::boxed(
                            "ResolvePersistentNotification",
                            "app.user.get.app_error",
                            None,
                            String::new(),
                            500,
                        )
                    }
                })?;
            if user.is_guest() {
                return Ok(());
            }
        }

        let mentioned = std::sync::atomic::AtomicBool::new(false);
        self.for_each_persistent_notification_post(
            std::slice::from_ref(post),
            Visit::IsMentioned {
                user_id: logged_in_user_id,
                mentioned: &mentioned,
            },
        )
        .await
        .map_err(|err| delete_error(&err))?;

        // Only mentioned users can stop the notifications.
        if !mentioned.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        self.store()
            .post()
            .delete_persistent_notification(&post.id)
            .await
            .map_err(|err| delete_error(&err))
    }

    /// Port of `App.DeletePersistentNotification` (post_persistent_notification.go:76).
    pub(crate) async fn retire_persistent_notification(
        &self,
        post: &Post,
    ) -> Result<(), Box<AppError>> {
        let delete_error = |err: &dyn std::fmt::Display| {
            tracing::error!(error = %err, "deleting the persistent notification failed");
            AppError::boxed(
                "DeletePersistentNotification",
                "app.post_priority.delete_persistent_notification_post.app_error",
                None,
                String::new(),
                500,
            )
        };
        if !self.is_persistent_notifications_enabled() {
            return Ok(());
        }
        if !self
            .store()
            .post()
            .has_persistent_notification(&post.id)
            .await
            .map_err(|err| delete_error(&err))?
        {
            return Ok(());
        }
        self.store()
            .post()
            .delete_persistent_notification(&post.id)
            .await
            .map_err(|err| delete_error(&err))
    }

    /// Port of `App.SendPersistentNotifications` (post_persistent_notification.go:98): the
    /// `post_persistent_notifications` job's body.
    ///
    /// Pages of 500 rows whose `CreateAt` **and** `LastSentAt` are at least one interval old and
    /// whose `SentCount` is below the maximum; each page's posts are notified, then every row of
    /// the page — whatever happened to its post — has `LastSentAt` moved to now and `SentCount`
    /// incremented, which is what moves it out of the next query's window. Rows that reached the
    /// maximum are retired at the end.
    pub async fn send_persistent_notifications(&self) -> Result<(), PersistentNotificationError> {
        let settings = self.persistent_settings().await;
        let max_count = i16::try_from(settings.max_count).unwrap_or(i16::MAX);
        let max_time = get_millis() - settings.interval_minutes * 60 * 1000;

        loop {
            let rows = self
                .store()
                .post()
                .get_due_persistent_notifications(max_time, max_count, PER_PAGE)
                .await
                .map_err(|err| {
                    PersistentNotificationError::wrap(
                        "failed to get posts for persistent notifications",
                        err,
                    )
                })?;
            if rows.is_empty() {
                break;
            }
            let post_ids: Vec<String> = rows.into_iter().map(|row| row.post_id).collect();
            let posts = self
                .store()
                .post()
                .get_posts_by_ids(&post_ids)
                .await
                .map_err(|err| {
                    PersistentNotificationError::wrap("failed to get posts by IDs", err)
                })?;

            self.for_each_persistent_notification_post(&posts, Visit::Send)
                .await?;

            self.store()
                .post()
                .update_persistent_notifications_last_activity(&post_ids)
                .await
                .map_err(|err| {
                    PersistentNotificationError::wrap(
                        format!("failed to update lastActivity for notifications: {post_ids:?}"),
                        err,
                    )
                })?;
        }

        self.store()
            .post()
            .delete_expired_persistent_notifications(max_count)
            .await
            .map_err(|err| {
                PersistentNotificationError::wrap("failed to delete expired notifications", err)
            })
    }

    /// Port of `App.forEachPersistentNotificationPost` (post_persistent_notification.go:153).
    pub(crate) async fn for_each_persistent_notification_post(
        &self,
        posts: &[Post],
        visit: Visit<'_>,
    ) -> Result<(), PersistentNotificationError> {
        let (channels, teams) = self.channel_team_maps_for_posts(posts).await?;
        let mut aux = self
            .persistent_notifications_auxiliary_data(&channels, &teams)
            .await?;

        let mut cleanup: Vec<&Post> = Vec::new();
        let empty_team = Team::default();
        for post in posts {
            if post.is_notification_suppressed() {
                cleanup.push(post);
                continue;
            }
            let Some(channel) = channels.get(&post.channel_id) else {
                cleanup.push(post);
                continue;
            };
            // GMs and DMs don't belong to any team.
            let team = if channel.is_group_or_direct() {
                &empty_team
            } else if let Some(team) = teams.get(&channel.team_id) {
                team
            } else {
                cleanup.push(post);
                continue;
            };

            let profile_map = aux.profiles.entry(channel.id.clone()).or_default();
            // Ensure the sender is always in the profile map: a system admin can post without
            // being a member.
            if !profile_map.contains_key(&post.user_id) {
                let sender = self
                    .store()
                    .user()
                    .get(&post.user_id)
                    .await
                    .map_err(|err| {
                        PersistentNotificationError::wrap(
                            format!(
                                "failed to get profile for sender user {} for post {}",
                                post.user_id, post.id
                            ),
                            err,
                        )
                    })?;
                profile_map.insert(post.user_id.clone(), sender);
            }

            let mut mentions = MentionResults::default();
            if channel.channel_type == CHANNEL_TYPE_DIRECT {
                // In DMs, only the "other" user can be mentioned.
                let other = channel.get_other_user_id_for_dm(&post.user_id);
                if profile_map.contains_key(other) {
                    mentions.add_mention(other, MentionType::DmMention);
                }
            } else {
                let empty_groups = BTreeMap::new();
                let groups = aux.groups.get(&channel.id).unwrap_or(&empty_groups);
                let mut keywords = aux.keywords.get(&channel.id).cloned().unwrap_or_default();
                keywords.add_groups_map(groups.values());
                mentions = get_explicit_mentions(
                    post,
                    &keywords,
                    self.config().feature_flags.mm_blocks_enabled,
                );
                let group_ids: Vec<String> = mentions.group_mentions.keys().cloned().collect();
                for group_id in group_ids {
                    // The keywords came from this map, so the group is in it.
                    let Some(group) = groups.get(&group_id) else {
                        continue;
                    };
                    self.insert_group_mentions(
                        &post.user_id,
                        group,
                        channel,
                        profile_map,
                        &mut mentions,
                    )
                    .await
                    .map_err(|err| {
                        PersistentNotificationError::wrap(
                            format!(
                                "failed to include mentions from group - {} for channel - {}",
                                group.id, channel.id
                            ),
                            AppErrorSource(err),
                        )
                    })?;
                }
            }

            let profile_map = &*profile_map;
            let notify_props = aux.notify_props.get(&channel.id);
            self.visit_persistent_notification_post(
                visit,
                post,
                channel,
                team,
                &mentions,
                profile_map,
                notify_props,
            )
            .await?;
        }

        for post in cleanup {
            if let Err(err) = self.retire_persistent_notification(post).await {
                tracing::warn!(
                    post_id = %post.id,
                    channel_id = %post.channel_id,
                    error = %err,
                    "Failed to delete persistent notification for post"
                );
            }
        }
        Ok(())
    }

    /// The closure body for each caller — see [`Visit`].
    #[allow(clippy::too_many_arguments)]
    async fn visit_persistent_notification_post(
        &self,
        visit: Visit<'_>,
        post: &Post,
        channel: &Channel,
        team: &Team,
        mentions: &MentionResults,
        profile_map: &BTreeMap<String, User>,
        notify_props: Option<&BTreeMap<String, StringMap>>,
    ) -> Result<(), PersistentNotificationError> {
        match visit {
            Visit::ValidateRecipients { max_recipients } => {
                let count = mentions.mentions.len() as i64;
                if count > max_recipients {
                    let params = std::collections::HashMap::from([(
                        "MaxRecipients".to_owned(),
                        serde_json::Value::from(max_recipients),
                    )]);
                    return Err(PersistentNotificationError::App(AppError::boxed(
                        "CreatePost",
                        "api.post.post_priority.max_recipients_persistent_notification_post.request_error",
                        Some(params),
                        String::new(),
                        400,
                    )));
                }
                if count == 0 {
                    return Err(PersistentNotificationError::App(AppError::boxed(
                        "CreatePost",
                        "api.post.post_priority.min_recipients_persistent_notification_post.request_error",
                        None,
                        String::new(),
                        400,
                    )));
                }
                Ok(())
            }
            Visit::IsMentioned { user_id, mentioned } => {
                if mentions.is_user_mentioned(user_id) {
                    mentioned.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(())
            }
            Visit::Send => {
                self.send_persistent_notifications_for_post(
                    post,
                    channel,
                    team,
                    mentions,
                    profile_map,
                    notify_props,
                )
                .await
            }
        }
    }

    /// Port of `App.channelTeamMapsForPosts` (post_persistent_notification.go:282):
    /// `GetChannelsByIds(ids, false)` — deleted channels are absent — and `Team().GetMany`, whose
    /// empty result is an error.
    async fn channel_team_maps_for_posts(
        &self,
        posts: &[Post],
    ) -> Result<(HashMap<String, Channel>, HashMap<String, Team>), PersistentNotificationError>
    {
        let mut channel_ids: Vec<String> = posts.iter().map(|p| p.channel_id.clone()).collect();
        channel_ids.sort();
        channel_ids.dedup();
        let channels = self
            .store()
            .channel()
            .get_many(&channel_ids)
            .await
            .map_err(|err| PersistentNotificationError::wrap("failed to get teams by IDs", err))?;
        let channels: HashMap<String, Channel> = channels
            .into_iter()
            .filter(|channel| channel.delete_at == 0)
            .map(|channel| (channel.id.clone(), channel))
            .collect();

        let mut team_ids: Vec<String> = channels
            .values()
            .filter(|channel| !channel.team_id.is_empty())
            .map(|channel| channel.team_id.clone())
            .collect();
        team_ids.sort();
        team_ids.dedup();
        let teams = if team_ids.is_empty() {
            Vec::new()
        } else {
            self.store()
                .team()
                .get_many(&team_ids)
                .await
                .map_err(|err| {
                    PersistentNotificationError::wrap("failed to get teams by IDs", err)
                })?
        };
        Ok((
            channels,
            teams
                .into_iter()
                .map(|team| (team.id.clone(), team))
                .collect(),
        ))
    }

    /// Port of `App.persistentNotificationsAuxiliaryData` (post_persistent_notification.go:239).
    async fn persistent_notifications_auxiliary_data(
        &self,
        channels: &HashMap<String, Channel>,
        teams: &HashMap<String, Team>,
    ) -> Result<AuxiliaryData, PersistentNotificationError> {
        let mut aux = AuxiliaryData::default();
        for channel in channels.values() {
            let team = teams.get(&channel.team_id);
            if team.is_none() && !channel.is_group_or_direct() {
                continue;
            }
            let profiles_error = |err: &dyn std::fmt::Display| {
                PersistentNotificationError::Message(format!(
                    "failed to get profiles for channel {}: {err}",
                    channel.id
                ))
            };

            // In a DM, notifications can't be sent to any third person.
            if channel.channel_type != CHANNEL_TYPE_DIRECT {
                let groups = self
                    .get_groups_allowed_for_reference_in_channel(channel, team)
                    .await
                    .map_err(|err| profiles_error(&err))?;
                aux.groups.insert(channel.id.clone(), groups);
                let props = self
                    .store()
                    .channel()
                    .get_all_channel_members_notify_props_for_channel(&channel.id, true)
                    .await
                    .map_err(|err| profiles_error(&err))?;
                aux.notify_props.insert(channel.id.clone(), props);
            }

            let profiles = self
                .store()
                .user()
                .get_all_profiles_in_channel(&channel.id, true)
                .await
                .map_err(|err| profiles_error(&err))?;
            let mut keywords = MentionKeywords::new();
            for (user_id, user) in &profiles {
                if !user.is_bot {
                    keywords.add_user_keyword(user_id, &format!("@{}", user.username));
                }
            }
            aux.keywords.insert(channel.id.clone(), keywords);
            aux.profiles.insert(channel.id.clone(), profiles);
        }
        Ok(aux)
    }

    /// Port of `App.insertGroupMentions` (notification.go:1567), for the part persistent
    /// notifications read: every member of the group other than the sender who is in the
    /// channel's profile map becomes a `GroupMention`. The out-of-channel members' usernames go
    /// to `other_potential_mentions`, as in Go, though nothing here reads them.
    pub(crate) async fn insert_group_mentions(
        &self,
        sender_id: &str,
        group: &Group,
        channel: &Channel,
        profile_map: &BTreeMap<String, User>,
        mentions: &mut MentionResults,
    ) -> Result<bool, Box<AppError>> {
        let is_group_or_direct = channel.is_group_or_direct();
        let members = if is_group_or_direct {
            self.store().group().get_member_users(&group.id).await
        } else {
            self.store()
                .group()
                .get_member_users_in_team(&group.id, &channel.team_id)
                .await
        }
        .map_err(|err| {
            tracing::error!(error = %err, "group member lookup failed");
            AppError::boxed(
                "insertGroupMentions",
                "app.select_error",
                None,
                String::new(),
                500,
            )
        })?;

        for member in &members {
            if member.id != sender_id {
                if profile_map.contains_key(&member.id) {
                    // A plain assignment in Go, not `addMention`: a group mention overrides.
                    mentions
                        .mentions
                        .insert(member.id.clone(), MentionType::GroupMention);
                } else {
                    mentions
                        .other_potential_mentions
                        .push(member.username.clone());
                }
            }
        }
        Ok(is_group_or_direct || !members.is_empty())
    }

    /// Port of `App.sendPersistentNotifications` (post_persistent_notification.go:316).
    ///
    /// The recipients are the mentioned users other than the author whose mention outranks a GM
    /// mention. More of them than `MaxNotificationsPerChannel` fails the run. Push goes to each
    /// recipient `ShouldSendPushNotification` accepts (as an explicit mention); the
    /// `persistent_notification_triggered` event goes to each whose desktop setting is not
    /// `none` and whose status is not DND or out of office, with the **prepared** post (priority
    /// included) and the list of those recipients.
    ///
    /// Go builds the recipient list from a map, so its order — and the `mentions` array's — is
    /// random there; it is sorted here.
    async fn send_persistent_notifications_for_post(
        &self,
        post: &Post,
        channel: &Channel,
        team: &Team,
        mentions: &MentionResults,
        profile_map: &BTreeMap<String, User>,
        notify_props: Option<&BTreeMap<String, StringMap>>,
    ) -> Result<(), PersistentNotificationError> {
        let mentioned: Vec<&String> = mentions
            .mentions
            .iter()
            .filter(|(id, kind)| **id != post.user_id && **kind > MentionType::GmMention)
            .map(|(id, _)| id)
            .collect();

        let Some(sender) = profile_map.get(&post.user_id) else {
            // `forEachPersistentNotificationPost` inserted the sender before calling this.
            return Err(PersistentNotificationError::Message(format!(
                "the sender {} of post {} is not in the profile map",
                post.user_id, post.id
            )));
        };
        let notification = PostNotification {
            post,
            channel,
            profile_map,
            sender,
        };

        let max = self.config().max_notifications_per_channel;
        if mentioned.len() as i64 > max {
            return Err(PersistentNotificationError::Message(format!(
                "mentioned users: {} are more than allowed users: {max}",
                mentioned.len()
            )));
        }

        if self.can_send_push_notifications().await {
            let is_gm = channel.channel_type == CHANNEL_TYPE_GROUP;
            for id in &mentioned {
                let Some(user) = profile_map.get(*id) else {
                    continue;
                };
                let status = self.status_or_offline(id).await;
                let props = notify_props.and_then(|props| props.get(*id));
                if Self::should_send_push_notification(user, props, true, &status, post, is_gm) {
                    self.send_push_notification(&notification, user, true, false, "")
                        .await;
                }
            }
        }

        let mut desktop_users: Vec<String> = Vec::new();
        for id in &mentioned {
            let Some(user) = profile_map.get(*id) else {
                continue;
            };
            let desktop = user
                .notify_props
                .as_ref()
                .and_then(|props| props.get(DESKTOP_NOTIFY_PROP))
                .map_or("", String::as_str);
            if desktop != USER_NOTIFY_NONE {
                let status = self.status_or_offline(id).await;
                if status.status != STATUS_DND && status.status != STATUS_OUT_OF_OFFICE {
                    desktop_users.push((*id).clone());
                }
            }
        }
        if desktop_users.is_empty() {
            return Ok(());
        }

        let prepared = self
            .prepare_post_for_client(
                post,
                PreparePostForClientOpts {
                    include_priority: true,
                    ..PreparePostForClientOpts::default()
                },
            )
            .await
            .map_err(|err| match err {
                PrepareError::Unreproducible(why) => {
                    PersistentNotificationError::Unreproducible(why)
                }
                other => PersistentNotificationError::wrap("failed to prepare the post", other),
            })?;
        let post_json = prepared.to_json().map_err(|err| {
            PersistentNotificationError::wrap("failed to encode post to JSON", err)
        })?;
        let channel_display_name = notification.get_channel_name(SHOW_USERNAME, "");
        let sender_name = notification
            .get_sender_name(SHOW_USERNAME, self.config().enable_post_username_override);
        let mentions_json = serde_json::to_string(&desktop_users).map_err(|err| {
            PersistentNotificationError::wrap("failed to encode the mentions", err)
        })?;

        for user_id in &desktop_users {
            let mut message = WebSocketEvent::new(
                WEBSOCKET_EVENT_PERSISTENT_NOTIFICATION_TRIGGERED,
                &team.id,
                &prepared.channel_id,
                user_id,
                None,
                "",
            );
            message.add("post", serde_json::Value::String(post_json.clone()));
            message.add(
                "channel_type",
                serde_json::Value::String(channel.channel_type.clone()),
            );
            message.add(
                "channel_display_name",
                serde_json::Value::String(channel_display_name.clone()),
            );
            message.add(
                "channel_name",
                serde_json::Value::String(channel.name.clone()),
            );
            message.add(
                "sender_name",
                serde_json::Value::String(sender_name.clone()),
            );
            message.add("team_id", serde_json::Value::String(team.id.clone()));
            if prepared
                .file_ids
                .as_ref()
                .is_some_and(|ids| !ids.is_empty())
            {
                message.add("otherFile", serde_json::Value::String("true".to_owned()));
                match self
                    .store()
                    .file_info()
                    .get_for_post(&prepared.id, false)
                    .await
                {
                    Ok(infos) => {
                        if infos.iter().any(|info| info.is_image()) {
                            message.add("image", serde_json::Value::String("true".to_owned()));
                        }
                    }
                    Err(err) => tracing::warn!(
                        post_id = %prepared.id,
                        error = %err,
                        "Unable to get fileInfo for push notifications."
                    ),
                }
            }
            message.add("mentions", serde_json::Value::String(mentions_json.clone()));
            self.publish(message).await;
        }
        Ok(())
    }

    /// `GetStatus`, with Go's offline stand-in when it fails.
    async fn status_or_offline(&self, user_id: &str) -> Status {
        match self.get_status(user_id).await {
            Ok(status) => status,
            Err(err) => {
                tracing::warn!(error = %err, "Unable to fetch online status");
                Status {
                    user_id: user_id.to_owned(),
                    status: STATUS_OFFLINE.to_owned(),
                    ..Status::default()
                }
            }
        }
    }
}

/// The four per-channel maps `persistentNotificationsAuxiliaryData` returns.
#[derive(Default)]
struct AuxiliaryData {
    groups: HashMap<String, BTreeMap<String, Group>>,
    profiles: HashMap<String, BTreeMap<String, User>>,
    keywords: HashMap<String, MentionKeywords>,
    notify_props: HashMap<String, BTreeMap<String, StringMap>>,
}

/// An `AppError` as an error source, so `errors.Wrapf(appErr, …)` keeps its cause.
#[derive(Debug)]
struct AppErrorSource(Box<AppError>);

impl std::fmt::Display for AppErrorSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for AppErrorSource {}
