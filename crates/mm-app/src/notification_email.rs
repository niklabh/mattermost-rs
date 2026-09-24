//! Port of the notification e-mail a post sends: `app/notification_email.go`
//! (`buildEmailNotification`, `sendNotificationEmail`, the three subject builders,
//! `getNotificationEmailBodyFromEmailNotification`), `App.userAllowsEmail` (notification.go:1161),
//! the half of `app/email/notification_email.go` the body needs (`GetMessageForNotification`,
//! `ProcessMessageAttachments`, `prepareTextForEmail`, `GenerateHyperlinkForChannels`) and
//! `utils.GetFormattedPostTime` (channels/utils/time.go:47).
//!
//! # What reaches the recipient
//!
//! One `messages_notification` body per recipient, rendered in the recipient's locale, with the
//! post's markdown turned into HTML by goldmark (`crate::markdown_utils::markdown_to_html`), the
//! `~channel` mentions of public channels turned into links, and the sender's avatar embedded as
//! `user-avatar.png` when there is a message to show it beside. The `Message-ID` is the post's id
//! at the site's host, and a reply carries `In-Reply-To`/`References` naming its root — so a mail
//! client threads the notifications the way the channel does.
//!
//! # What is not here
//!
//! Email **batching** (`EnableEmailBatching`, off by default): a batched recipient is sent the
//! single mail instead, and the divergence is [D-1072]. A sender with **no stored picture**: Go
//! generates the initials avatar and writes it; this server sends the mail without the photo
//! ([D-1072] too).

use std::collections::BTreeMap;

use gotemplate::Value;
use mm_model::channel::{CHANNEL_TYPE_DIRECT, CHANNEL_TYPE_GROUP, CHANNEL_TYPE_OPEN};
use mm_model::email_notification::{EmailNotification, EmailNotificationContent};
use mm_model::post::Post;
use mm_model::team::Team;
use mm_model::user::User;
use mm_store::{ChannelStore, FileInfoStore, PreferenceStore, TeamStore};

use crate::App;
use crate::email::{EmailError, Tr, user_translations};
use crate::notification::PostNotification;

/// `model.EmailNotificationContentsFull`.
const EMAIL_NOTIFICATION_CONTENTS_FULL: &str = "full";

/// Port of `utils.FormattedPostTime` (channels/utils/time.go:34).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormattedPostTime {
    pub year: String,
    pub month: String,
    pub day: String,
    pub hour: String,
    pub minute: String,
    pub time_zone: String,
}

/// Go's `time.Local`: `$TZ` when set (empty meaning UTC), else the zone `/etc/localtime` links to.
fn go_local_zone() -> chrono_tz::Tz {
    match std::env::var("TZ") {
        Ok(tz) if tz.is_empty() => chrono_tz::UTC,
        Ok(tz) => tz.trim_start_matches(':').parse().unwrap_or(chrono_tz::UTC),
        Err(_) => std::fs::read_link("/etc/localtime")
            .ok()
            .and_then(|target| {
                let target = target.to_string_lossy().into_owned();
                target
                    .split_once("zoneinfo/")
                    .map(|(_, name)| name.to_owned())
            })
            .and_then(|name| name.parse().ok())
            .unwrap_or(chrono_tz::UTC),
    }
}

/// Port of `utils.GetFormattedPostTime` (channels/utils/time.go:47).
///
/// The post's time is taken to the **second** (`time.Unix(CreateAt/1000, 0)`), in the recipient's
/// preferred zone when it names one Go can load and in the server's zone otherwise. The month is
/// **translated** — its English name is itself a translation id — and the zone is the
/// abbreviation (`IST`, `UTC`), not an offset.
pub fn get_formatted_post_time(
    user: &User,
    post: &Post,
    use_military_time: bool,
    t: &Tr,
) -> FormattedPostTime {
    use chrono::{Datelike, Timelike};
    let preferred = user.get_preferred_timezone();
    let zone: chrono_tz::Tz = if preferred.is_empty() {
        go_local_zone()
    } else {
        preferred.parse().unwrap_or_else(|_| go_local_zone())
    };
    let utc = chrono::DateTime::from_timestamp(post.create_at / 1000, 0).unwrap_or_default();
    let local = utc.with_timezone(&zone);

    let (hour, period) = if use_military_time {
        (format!("{:02}", local.hour()), String::new())
    } else {
        let (pm, hour12) = local.hour12();
        (
            hour12.to_string(),
            if pm {
                " PM".to_owned()
            } else {
                " AM".to_owned()
            },
        )
    };
    FormattedPostTime {
        year: local.year().to_string(),
        month: t.t(&local.format("%B").to_string()),
        day: local.day().to_string(),
        hour,
        minute: format!("{:02}{period}", local.minute()),
        time_zone: local.format("%Z").to_string(),
    }
}

/// `html.EscapeString` — the five characters `EscapeString` rewrites.
fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&#39;"),
            '"' => out.push_str("&#34;"),
            other => out.push(other),
        }
    }
    out
}

/// `template.HTMLEscapeString` — `html.EscapeString` plus NUL to U+FFFD.
fn template_html_escape(text: &str) -> String {
    html_escape(text).replace('\0', "\u{FFFD}")
}

/// Port of `prepareTextForEmail` (email/notification_email.go:131): escape, then markdown to
/// HTML; a markdown failure falls back to the **unescaped** text.
fn prepare_text_for_email(text: &str, site_url: &str) -> Value {
    match crate::markdown_utils::markdown_to_html(&html_escape(text), site_url) {
        Ok(html) => Value::Html(html),
        Err(err) => {
            tracing::warn!(error = %err, "Encountered error while converting markdown to HTML");
            Value::Html(text.to_owned())
        }
    }
}

impl App {
    /// Port of `App.userAllowsEmail` (notification.go:1161).
    ///
    /// # Four ways to refuse, and the channel setting is not always one of them
    ///
    /// A bot or remote recipient never gets mail; neither does anyone for an access-control
    /// team-membership notice. The user's `email` prop is the default, the channel member's
    /// overrides it **unless** collapsed threads are on for the user and the post is a reply; a
    /// muted channel removes the recipient. And the recipient must not be online or DND, must not
    /// be deactivated, and neither they nor the post may be out-of-office related.
    pub(crate) async fn user_allows_email(
        &self,
        user: &User,
        channel_member_props: Option<&mm_model::utils::StringMap>,
        post: &Post,
    ) -> bool {
        if user.is_bot || user.is_remote() {
            return false;
        }
        if post.is_access_control_team_membership_notification() {
            return false;
        }
        let user_prop = |key: &str| {
            user.notify_props
                .as_ref()
                .and_then(|p| p.get(key))
                .map(String::as_str)
        };
        let mut allows = user_prop(mm_model::user::EMAIL_NOTIFY_PROP) != Some("false");
        let member = |key: &str| {
            channel_member_props
                .and_then(|p| p.get(key))
                .map(String::as_str)
        };
        if let Some(channel_email) = member(mm_model::user::EMAIL_NOTIFY_PROP)
            && !(self.is_crt_enabled_for_user(&user.id).await && !post.root_id.is_empty())
            && channel_email != mm_model::channel_member::CHANNEL_NOTIFY_DEFAULT
        {
            allows = channel_email != "false";
        }
        if member("mark_unread") == Some("mention") {
            allows = false;
        }
        let status = self
            .get_status(&user.id)
            .await
            .unwrap_or_else(|_| mm_model::status::Status {
                user_id: user.id.clone(),
                status: mm_model::status::STATUS_OFFLINE.to_owned(),
                ..mm_model::status::Status::default()
            });
        let auto_responder_related =
            status.status == "ooo" || post.post_type == "system_auto_responder";
        let allowed_for_status =
            status.status != mm_model::status::STATUS_ONLINE && status.status != "dnd";
        allows && allowed_for_status && user.delete_at == 0 && !auto_responder_related
    }

    /// Port of `Service.GetMessageForNotification` (email/notification_email.go:30): the post's
    /// markdown as HTML, or — for a post that is **only** files — a translated sentence naming
    /// them, its parameters HTML-escaped (`i18n.TranslateAsHTML`).
    pub(crate) async fn get_message_for_notification(
        &self,
        post: &Post,
        team_name: &str,
        site_url: &str,
        t: &Tr,
    ) -> String {
        let file_ids = post.file_ids.as_deref().unwrap_or_default();
        if !post.message.trim().is_empty() || file_ids.is_empty() {
            return self
                .prepare_notification_message_for_email(&post.message, team_name, site_url)
                .await;
        }
        let infos = match self.store().file_info().get_for_post(&post.id, false).await {
            Ok(infos) => infos,
            Err(err) => {
                tracing::warn!(post_id = %post.id, error = %err, "Encountered error when getting files for notification message");
                Vec::new()
            }
        };
        let mut only_images = true;
        let filenames: Vec<String> = infos
            .iter()
            .map(|info| {
                only_images = only_images && info.is_image();
                mm_model::go_url::query_unescape(&info.name).unwrap_or_else(|_| info.name.clone())
            })
            .collect();
        let id = if only_images {
            "api.post.get_message_for_notification.images_sent"
        } else {
            "api.post.get_message_for_notification.files_sent"
        };
        let message = t.tp(
            id,
            &[
                ("Count", serde_json::Value::from(filenames.len())),
                (
                    "Filenames",
                    serde_json::Value::String(template_html_escape(&filenames.join(", "))),
                ),
            ],
        );
        message.replace("[[", "<strong>").replace("]]", "</strong>")
    }

    /// Port of `Service.prepareNotificationMessageForEmail` (email/notification_email.go:142).
    async fn prepare_notification_message_for_email(
        &self,
        post_message: &str,
        team_name: &str,
        site_url: &str,
    ) -> String {
        let escaped = html_escape(post_message);
        let md = match crate::markdown_utils::markdown_to_html(&escaped, site_url) {
            Ok(html) => html,
            Err(err) => {
                tracing::warn!(error = %err, "Encountered error while converting markdown to HTML");
                escaped
            }
        };
        let landing_url = format!("{site_url}/landing#/{team_name}");
        match self
            .generate_hyperlink_for_channels(&md, team_name, &landing_url)
            .await
        {
            Ok(linked) => linked,
            Err(err) => {
                tracing::warn!(team_name, error = %err, "Encountered error while generating hyperlink for channels");
                md
            }
        }
    }

    /// Port of `Service.GenerateHyperlinkForChannels` (email/notification_email.go:159): every
    /// `~name` of an **open** channel in the team becomes a link, every occurrence of it.
    pub(crate) async fn generate_hyperlink_for_channels(
        &self,
        post_message: &str,
        team_name: &str,
        landing_url: &str,
    ) -> Result<String, mm_store::StoreError> {
        let channel_names = mm_model::channel_mentions::channel_mentions(post_message);
        if channel_names.is_empty() {
            return Ok(post_message.to_owned());
        }
        let Ok(team) = self.store().team().get_by_name(team_name).await else {
            tracing::error!(team_name, "Team not found with the name");
            return Ok(post_message.to_owned());
        };
        let channels = self
            .store()
            .channel()
            .get_by_names(&team.id, &channel_names)
            .await?;
        let mut message = post_message.to_owned();
        let mut visited = std::collections::HashSet::new();
        for channel in channels {
            if channel.channel_type == CHANNEL_TYPE_OPEN && visited.insert(channel.id.clone()) {
                let link = format!(
                    "<a href='{landing_url}/channels/{name}'>~{name}</a>",
                    name = channel.name
                );
                message = message.replace(&format!("~{}", channel.name), &link);
            }
        }
        Ok(message)
    }

    /// Port of `App.buildEmailNotification` (app/notification_email.go:24).
    ///
    /// `EmailNotificationContentsType` is honoured only under a licence with the
    /// `email_notification_contents` feature; everyone else gets the full contents.
    async fn build_email_notification(
        &self,
        notification: &PostNotification<'_>,
        user: &User,
        team: &Team,
        config: &mm_model::config::Config,
    ) -> Result<EmailNotification, EmailError> {
        let channel = notification.channel;
        let post = notification.post;
        let sender = notification.sender;
        let t = user_translations(&user.locale).await?;
        let name_format = self.get_notification_name_format(user).await;
        let use_military_time = match self
            .store()
            .preference()
            .get(&user.id, "display_settings", "use_military_time")
            .await
        {
            Ok(preference) => preference.value == "true",
            Err(_) => false,
        };
        let channel_name = notification.get_channel_name(&name_format, "");
        let sender_name =
            notification.get_sender_name(&name_format, self.config().enable_post_username_override);

        let licensed_contents = self.license().await.ok().flatten().is_some_and(|l| {
            l.features
                .as_ref()
                .and_then(|f| f.email_notification_contents)
                .unwrap_or(false)
        });
        let contents_type = if licensed_contents {
            config
                .email_settings
                .email_notification_contents_type
                .clone()
                .unwrap_or_default()
        } else {
            EMAIL_NOTIFICATION_CONTENTS_FULL.to_owned()
        };

        let site_name = config.team_settings.site_name.clone().unwrap_or_default();
        let time = get_formatted_post_time(user, post, use_military_time, &t);
        let s = |v: &str| serde_json::Value::String(v.to_owned());
        let date = [
            ("Month", s(&time.month)),
            ("Day", s(&time.day)),
            ("Year", s(&time.year)),
        ];
        let subject = if channel.channel_type == CHANNEL_TYPE_DIRECT {
            let mut params = vec![
                ("SiteName", s(&site_name)),
                ("SenderDisplayName", s(&sender_name)),
            ];
            params.extend(date.iter().cloned());
            t.tp("app.notification.subject.direct.full", &params)
        } else if channel.channel_type == CHANNEL_TYPE_GROUP {
            let mut params = vec![("SiteName", s(&site_name))];
            params.extend(date.iter().cloned());
            if contents_type == EMAIL_NOTIFICATION_CONTENTS_FULL {
                params.push(("ChannelName", s(&channel_name)));
                t.tp("app.notification.subject.group_message.full", &params)
            } else {
                t.tp("app.notification.subject.group_message.generic", &params)
            }
        } else {
            let team_name = if config
                .email_settings
                .use_channel_in_email_notifications
                .unwrap_or(false)
            {
                format!("{} ({channel_name})", team.display_name)
            } else {
                team.display_name.clone()
            };
            let mut params = vec![("SiteName", s(&site_name)), ("TeamName", s(&team_name))];
            params.extend(date.iter().cloned());
            t.tp("app.notification.subject.notification.full", &params)
        };

        let sender_param = [("SenderName", s(&sender_name))];
        let (mut title, mut sub_title) = if channel.channel_type == CHANNEL_TYPE_DIRECT {
            (
                t.tp("app.notification.body.dm.title", &sender_param),
                t.tp("app.notification.body.dm.subTitle", &sender_param),
            )
        } else if channel.channel_type == CHANNEL_TYPE_GROUP {
            (
                t.tp("app.notification.body.group.title", &sender_param),
                t.tp("app.notification.body.group.subTitle", &sender_param),
            )
        } else {
            (
                t.tp("app.notification.body.mention.title", &sender_param),
                t.tp(
                    "app.notification.body.mention.subTitle",
                    &[
                        ("SenderName", s(&sender_name)),
                        ("ChannelName", s(&channel_name)),
                    ],
                ),
            )
        };
        let is_crt = self.is_crt_enabled_for_user(&user.id).await;
        if is_crt && !post.root_id.is_empty() {
            title = t.tp("app.notification.body.thread.title", &sender_param);
            sub_title = if channel.channel_type == CHANNEL_TYPE_DIRECT {
                t.tp("app.notification.body.thread_dm.subTitle", &sender_param)
            } else if channel.channel_type == CHANNEL_TYPE_GROUP {
                t.tp("app.notification.body.thread_gm.subTitle", &sender_param)
            } else if contents_type == EMAIL_NOTIFICATION_CONTENTS_FULL {
                t.tp(
                    "app.notification.body.thread_channel_full.subTitle",
                    &[
                        ("SenderName", s(&sender_name)),
                        ("ChannelName", s(&channel_name)),
                    ],
                )
            } else {
                t.tp(
                    "app.notification.body.thread_channel.subTitle",
                    &sender_param,
                )
            };
        }

        let site_url = config.service_settings.site_url.clone().unwrap_or_default();
        let (message_html, message_text) = if contents_type == EMAIL_NOTIFICATION_CONTENTS_FULL {
            (
                self.get_message_for_notification(post, &team.name, &site_url, &t)
                    .await,
                post.message.clone(),
            )
        } else {
            (String::new(), String::new())
        };
        let landing_url = format!("{site_url}/landing#/{}", team.name);
        let button_url = if team.name != "select_team" {
            format!("{landing_url}/pl/{}", post.id)
        } else {
            landing_url
        };

        Ok(EmailNotification {
            post_id: post.id.clone(),
            channel_id: channel.id.clone(),
            team_id: team.id.clone(),
            sender_id: sender.id.clone(),
            sender_display_name: sender_name,
            recipient_id: user.id.clone(),
            root_id: post.root_id.clone(),
            channel_type: channel.channel_type.clone(),
            channel_name,
            team_name: team.display_name.clone(),
            sender_username: sender.username.clone(),
            is_direct_message: channel.channel_type == CHANNEL_TYPE_DIRECT,
            is_group_message: channel.channel_type == CHANNEL_TYPE_GROUP,
            is_thread_reply: !post.root_id.is_empty(),
            is_crt_enabled: is_crt,
            use_military_time,
            content: EmailNotificationContent {
                subject,
                title,
                sub_title,
                message_html,
                message_text,
                button_text: t.t("api.templates.post_body.button"),
                button_url,
                footer_text: t.t("app.notification.footer.title"),
            },
        })
    }

    /// `RunMultiHook(EmailNotificationWillBeSent)`: `None` when a plugin rejected the mail.
    async fn run_email_notification_will_be_sent(
        &self,
        mut notification: EmailNotification,
    ) -> Option<EmailNotification> {
        let Some(environment) = self.hook_environment() else {
            return Some(notification);
        };
        for (hooks, manifest) in
            environment.hooks_implementing(mm_plugin::rpc::hook_id::EMAIL_NOTIFICATION_WILL_BE_SENT)
        {
            let returns = hooks
                .email_notification_will_be_sent(
                    mm_plugin::wire::plugin::Z_EmailNotificationWillBeSentArgs {
                        a: Some(Box::new(email_notification_to_wire(&notification))),
                    },
                )
                .await;
            if !returns.b.is_empty() {
                tracing::info!(rejection_reason = %returns.b, plugin_id = %manifest.id, "Email notification cancelled by plugin.");
                return None;
            }
            if let Some(content) = returns.a.as_deref() {
                notification.content = EmailNotificationContent {
                    subject: content.subject.clone(),
                    title: content.title.clone(),
                    sub_title: content.sub_title.clone(),
                    message_html: content.message_html.clone(),
                    message_text: content.message_text.clone(),
                    button_text: content.button_text.clone(),
                    button_url: content.button_url.clone(),
                    footer_text: content.footer_text.clone(),
                };
                tracing::info!(plugin_id = %manifest.id, "Email notification modified by plugin.");
            }
        }
        Some(notification)
    }

    /// Port of `App.sendNotificationEmail` (app/notification_email.go:143), minus batching
    /// ([D-1072]). The mail itself goes out from a background task, as Go's `Srv().Go` does.
    pub(crate) async fn send_notification_email(
        &self,
        notification: &PostNotification<'_>,
        user: &User,
        team: &Team,
        sender_profile_image: Option<Vec<u8>>,
    ) -> Result<(), EmailError> {
        let config = crate::config::load_model_config(self.store().config()).await?;
        let site_name = config.team_settings.site_name.clone().unwrap_or_default();
        let channel = notification.channel;
        let post = notification.post;

        let mut team = team.clone();
        if channel.channel_type == CHANNEL_TYPE_DIRECT || channel.channel_type == CHANNEL_TYPE_GROUP
        {
            let teams = self.store().team().get_teams_by_user_id(&user.id).await?;
            let found = teams.iter().any(|t| t.id == team.id);
            // Go's `if !found && len(teams) > 0 { … } else { select_team }` — so a recipient who
            // **is** in the post's team is also sent to `select_team`. Reproduced.
            if !found && !teams.is_empty() {
                team = teams[0].clone();
            } else {
                team = Team {
                    name: "select_team".to_owned(),
                    display_name: site_name.clone(),
                    ..Team::default()
                };
            }
        }

        let email_notification = self
            .build_email_notification(notification, user, &team, &config)
            .await?;
        let Some(email_notification) = self
            .run_email_notification_will_be_sent(email_notification)
            .await
        else {
            return Ok(());
        };

        if config.email_settings.enable_email_batching.unwrap_or(false) {
            tracing::warn!(user_id = %user.id, "email batching is not ported; sending the single notification (D-1072)");
        }

        let (sender_photo, embedded) = match sender_profile_image {
            Some(image) if !email_notification.content.message_html.is_empty() => (
                "user-avatar.png".to_owned(),
                vec![("user-avatar.png".to_owned(), image)],
            ),
            _ => (String::new(), Vec::new()),
        };

        let body = self
            .notification_email_body(user, &email_notification, post, &sender_photo, &config)
            .await?;

        let site_url = config.service_settings.site_url.clone().unwrap_or_default();
        let host = mm_model::go_url::go_parse(&site_url)
            .map(|u| String::from_utf8_lossy(&u.hostname()).into_owned())
            .unwrap_or_default();
        let message_id = if email_notification.post_id.is_empty() {
            String::new()
        } else {
            format!("<{}@{host}>", email_notification.post_id)
        };
        let references = if email_notification.root_id.is_empty() {
            String::new()
        } else {
            format!("<{}@{host}>", email_notification.root_id)
        };
        let subject = mm_model::go_html::unescape_string(&email_notification.content.subject);

        let app = self.clone();
        let to = user.email.clone();
        tokio::spawn(async move {
            if let Err(err) = app
                .send_mail_with_embedded_files(
                    &to,
                    &subject,
                    &body,
                    &embedded,
                    &message_id,
                    &references,
                    &references,
                    "Notification",
                )
                .await
            {
                tracing::error!(error = %err, "Error while sending the email");
            }
        });
        Ok(())
    }

    /// Port of `App.getNotificationEmailBodyFromEmailNotification` (app/notification_email.go:330).
    async fn notification_email_body(
        &self,
        recipient: &User,
        email_notification: &EmailNotification,
        post: &Post,
        sender_photo: &str,
        config: &mm_model::config::Config,
    ) -> Result<String, EmailError> {
        let t = user_translations(&recipient.locale).await?;
        let site_url = config.service_settings.site_url.clone().unwrap_or_default();
        let content = &email_notification.content;

        let mut message = Value::Html(String::new());
        let mut time = String::new();
        let mut attachments = Vec::new();
        if !content.message_html.is_empty() {
            message = Value::Html(content.message_html.clone());
            let formatted =
                get_formatted_post_time(recipient, post, email_notification.use_military_time, &t);
            time = t.tp(
                "app.notification.body.dm.time",
                &[
                    ("Hour", serde_json::Value::String(formatted.hour)),
                    ("Minute", serde_json::Value::String(formatted.minute)),
                    ("TimeZone", serde_json::Value::String(formatted.time_zone)),
                ],
            );
            attachments = process_message_attachments(post, &site_url);
        }
        let channel_name =
            if email_notification.is_direct_message || email_notification.is_group_message {
                String::new()
            } else {
                email_notification.channel_name.clone()
            };
        let post_data = Value::Struct(
            "app.postData".to_owned(),
            vec![
                (
                    "SenderName".to_owned(),
                    Value::String(truncate_user_names(
                        &email_notification.sender_display_name,
                        22,
                    )),
                ),
                ("ChannelName".to_owned(), Value::String(channel_name)),
                ("Message".to_owned(), message),
                ("MessageURL".to_owned(), Value::String(String::new())),
                (
                    "SenderPhoto".to_owned(),
                    Value::String(sender_photo.to_owned()),
                ),
                ("PostPhoto".to_owned(), Value::String(String::new())),
                ("Time".to_owned(), Value::String(time)),
                ("ShowChannelIcon".to_owned(), Value::Bool(false)),
                ("OtherChannelMembersCount".to_owned(), Value::Int(0)),
                ("MessageAttachments".to_owned(), Value::List(attachments)),
            ],
        );

        let mut data = self.new_email_template_data_for(&recipient.locale).await?;
        data.set("SiteURL", site_url.as_str());
        data.set("ButtonURL", content.button_url.as_str());
        data.set(
            "SenderName",
            email_notification.sender_display_name.as_str(),
        );
        data.set("Button", content.button_text.as_str());
        data.set("NotificationFooterTitle", content.footer_text.as_str());
        data.set(
            "NotificationFooterInfoLogin",
            t.t("app.notification.footer.infoLogin"),
        );
        data.set(
            "NotificationFooterInfo",
            t.t("app.notification.footer.info"),
        );
        data.set("Title", content.title.as_str());
        data.set("SubTitle", content.sub_title.as_str());
        let posts = if content.message_html.is_empty() {
            Vec::new()
        } else {
            vec![post_data]
        };
        data.props.insert("Posts".to_owned(), Value::List(posts));
        crate::email::render_template("messages_notification", data).await
    }
}

/// Port of `truncateUserNames` (app/notification_email.go:307): more than `i` **runes** become the
/// first `i` and `...`.
fn truncate_user_names(name: &str, i: usize) -> String {
    if name.chars().count() > i {
        let mut out: String = name.chars().take(i).collect();
        out.push_str("...");
        out
    } else {
        name.to_owned()
    }
}

/// Port of `ProcessMessageAttachments` (email/notification_email.go:67), as the template sees the
/// result: each `*EmailMessageAttachment` with the embedded `model.MessageAttachment` flattened and
/// its `Pretext` and `Text` shadowed by the HTML versions, the title markdown-stripped, and the
/// fields laid into rows — a long field alone, short ones two to a row.
fn process_message_attachments(post: &Post, site_url: &str) -> Vec<Value> {
    post.attachments()
        .into_iter()
        .map(|attachment| {
            let title = match crate::markdown_utils::strip_markdown(&attachment.title) {
                Ok(stripped) => stripped,
                Err(err) => {
                    tracing::warn!(post_id = %post.id, error = %err, "Failed parse to markdown from messageatatchment title");
                    String::new()
                }
            };
            let mut rows: Vec<Value> = Vec::new();
            let mut short_row: Vec<Value> = Vec::new();
            let row = |cells: Vec<Value>| {
                Value::Struct("email.FieldRow".to_owned(), vec![("Cells".to_owned(), Value::List(cells))])
            };
            for field in attachment.fields.as_deref().unwrap_or_default() {
                let value = match &field.value {
                    serde_json::Value::String(text) => prepare_text_for_email(text, site_url),
                    other => json_to_value(other),
                };
                let cell = Value::Ptr(Box::new(Value::Struct(
                    "model.MessageAttachmentField".to_owned(),
                    vec![
                        ("Title".to_owned(), Value::String(field.title.clone())),
                        ("Value".to_owned(), value),
                        ("Short".to_owned(), Value::Bool(field.short.0)),
                    ],
                )));
                if !field.short.0 {
                    if !short_row.is_empty() {
                        rows.push(row(std::mem::take(&mut short_row)));
                    }
                    rows.push(row(vec![cell]));
                } else {
                    short_row.push(cell);
                    if short_row.len() == 2 {
                        rows.push(row(std::mem::take(&mut short_row)));
                    }
                }
            }
            if !short_row.is_empty() {
                rows.push(row(short_row));
            }
            Value::Ptr(Box::new(Value::Struct(
                "email.EmailMessageAttachment".to_owned(),
                vec![
                    ("Id".to_owned(), Value::Int(attachment.id)),
                    ("Fallback".to_owned(), Value::String(attachment.fallback.clone())),
                    ("Color".to_owned(), Value::String(attachment.color.clone())),
                    ("AuthorName".to_owned(), Value::String(attachment.author_name.clone())),
                    ("AuthorLink".to_owned(), Value::String(attachment.author_link.clone())),
                    ("AuthorIcon".to_owned(), Value::String(attachment.author_icon.clone())),
                    ("Title".to_owned(), Value::String(title)),
                    ("TitleLink".to_owned(), Value::String(attachment.title_link.clone())),
                    ("ImageURL".to_owned(), Value::String(attachment.image_url.clone())),
                    ("ThumbURL".to_owned(), Value::String(attachment.thumb_url.clone())),
                    ("Footer".to_owned(), Value::String(attachment.footer.clone())),
                    ("FooterIcon".to_owned(), Value::String(attachment.footer_icon.clone())),
                    ("Pretext".to_owned(), prepare_text_for_email(&attachment.pretext, site_url)),
                    ("Text".to_owned(), prepare_text_for_email(&attachment.text, site_url)),
                    ("FieldRows".to_owned(), Value::List(rows)),
                ],
            )))
        })
        .collect()
}

/// A JSON value as the template sees the `any` it came from.
fn json_to_value(value: &serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map_or_else(|| Value::Float(n.as_f64().unwrap_or_default()), Value::Int),
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(items) => Value::List(items.iter().map(json_to_value).collect()),
        serde_json::Value::Object(map) => Value::Map(
            map.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect::<BTreeMap<_, _>>(),
        ),
    }
}

/// `model.EmailNotification` as gob carries it to `EmailNotificationWillBeSent`.
fn email_notification_to_wire(n: &EmailNotification) -> mm_plugin::wire::model::EmailNotification {
    mm_plugin::wire::model::EmailNotification {
        post_id: n.post_id.clone(),
        channel_id: n.channel_id.clone(),
        team_id: n.team_id.clone(),
        sender_id: n.sender_id.clone(),
        sender_display_name: n.sender_display_name.clone(),
        recipient_id: n.recipient_id.clone(),
        root_id: n.root_id.clone(),
        channel_type: n.channel_type.clone(),
        channel_name: n.channel_name.clone(),
        team_name: n.team_name.clone(),
        sender_username: n.sender_username.clone(),
        is_direct_message: n.is_direct_message,
        is_group_message: n.is_group_message,
        is_thread_reply: n.is_thread_reply,
        is_crt_enabled: n.is_crt_enabled,
        use_military_time: n.use_military_time,
        email_notification_content: mm_plugin::wire::model::EmailNotificationContent {
            subject: n.content.subject.clone(),
            title: n.content.title.clone(),
            sub_title: n.content.sub_title.clone(),
            message_html: n.content.message_html.clone(),
            message_text: n.content.message_text.clone(),
            button_text: n.content.button_text.clone(),
            button_url: n.content.button_url.clone(),
            footer_text: n.content.footer_text.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Twenty-two runes, not bytes: a multi-byte name is cut by characters.
    #[test]
    fn user_names_truncate_by_runes() {
        assert_eq!(truncate_user_names("short", 22), "short");
        let long = "é".repeat(23);
        assert_eq!(
            truncate_user_names(&long, 22),
            format!("{}...", "é".repeat(22))
        );
        assert_eq!(truncate_user_names(&"é".repeat(22), 22), "é".repeat(22));
    }

    #[test]
    fn the_two_escapers_differ_only_on_nul() {
        assert_eq!(
            html_escape(r#"<a href="x">&'</a>"#),
            "&lt;a href=&#34;x&#34;&gt;&amp;&#39;&lt;/a&gt;"
        );
        assert_eq!(template_html_escape("a\0b"), "a\u{FFFD}b");
        assert_eq!(html_escape("a\0b"), "a\0b");
    }
}
