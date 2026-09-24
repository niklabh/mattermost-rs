//! Port of `channels/app/notification_push.go` — the push-notification hub and everything it
//! sends through the push proxy: the badge-clearing `clear`, the badge-only `update_badge`, the
//! post `message`, the device's `ack`, and the console's `test`.
//!
//! # The hub
//!
//! Go keeps one `PushNotificationsHub` per server: a buffered channel read by one goroutine that
//! starts a goroutine per notification, at most `NumCPU * 8` at once. Nothing waits for a result;
//! a failure is a log line. Here each notification is a task holding a permit of one process-wide
//! semaphore of the same size — the same bound and the same fire-and-forget contract. The
//! channel's buffer only decides when a producer *blocks*, which no response depends on.
//!
//! # The push proxy's view
//!
//! One `POST {PushNotificationServer}/api/v1/send_push` per live device session, whose body is
//! `json.Marshal(PushNotification)` — no `Content-Type` (Go's `http.NewRequest` sets none), and no
//! `X-Mattermost-Auth`: that header comes from `Srv().PushProxy`, an Enterprise interface that is
//! nil on every build this project can run, so Go never sends it either. Each message carries an
//! `ack_id` and an ES256 JWT over it (`signature`) signed with the installation's
//! `AsymmetricSigningKey`; both are fresh per send, and ECDSA signatures are randomised in Go, so
//! parity compares them by verifying, not by bytes.
//!
//! # What is not here
//!
//! Metrics (`CountNotificationReason` and friends are Prometheus counters this port has no
//! registry for) and the notification logger's per-reason lines, which are logs.

use std::sync::{Arc, OnceLock};

use mm_model::push_notification::{
    PUSH_MESSAGE_V2, PUSH_TYPE_CLEAR, PUSH_TYPE_MESSAGE, PUSH_TYPE_TEST, PUSH_TYPE_UPDATE_BADGE,
    PushNotification, PushNotificationAck, PushTransport,
};
use mm_model::push_response::{
    PUSH_STATUS, PUSH_STATUS_ERROR_MSG, PUSH_STATUS_FAIL, PUSH_STATUS_REMOVE,
};
use mm_model::session::Session;
use mm_model::utils::{AppError, AppResult};
use mm_store::{SessionStore, ThreadStore, UserStore};

use crate::App;

/// `model.SessionPropLastRemovedDeviceId`.
const PROP_LAST_REMOVED_DEVICE_ID: &str = "last_removed_device_id";
/// `model.SessionPropLastRemovedVoIPDeviceId`.
const PROP_LAST_REMOVED_VOIP_DEVICE_ID: &str = "last_removed_voip_device_id";
/// `notificationErrorRemoveDevice` — compared by text in Go, so kept as the text.
const ERROR_REMOVE_DEVICE: &str = "device was reported as removed";
/// `model.APIURLSuffixV1`.
const API_URL_SUFFIX_V1: &str = "/api/v1";

/// A failure talking to the push proxy. `Display` is Go's `err.Error()`, which is what reaches
/// the log and, for a `fail` status, what the proxy said.
#[derive(Debug, thiserror::Error)]
pub enum PushError {
    #[error("failed to encode to JSON: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("{0}")]
    Transport(String),
    #[error("response returned error code: {0}")]
    Status(u16),
    #[error("failed to decode from JSON: {0}")]
    Decode(String),
    /// `errors.New(pushResponse[model.PushStatusErrorMsg])` — the proxy's own words.
    #[error("{0}")]
    Proxy(String),
    #[error("{ERROR_REMOVE_DEVICE}")]
    RemoveDevice,
    #[error("Failed to set extra session properties: {0}")]
    SessionProps(String),
}

/// The hub's concurrency bound: `runtime.NumCPU() * 8`.
fn semaphore() -> Arc<tokio::sync::Semaphore> {
    static SEMA: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    Arc::clone(SEMA.get_or_init(|| {
        let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        Arc::new(tokio::sync::Semaphore::new(cpus * 8))
    }))
}

/// `s.pushNotificationClient` — `httpService.MakeClient(true)`: trusted URLs, so no outbound
/// guard, and the service's 30-second request timeout.
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_default()
    })
}

/// One queued notification, as `PushNotification` (the hub's own struct, not the model's).
enum Job {
    Clear {
        current_session_id: String,
        user_id: String,
        channel_id: String,
        root_id: String,
    },
    UpdateBadge {
        user_id: String,
    },
    /// `notificationTypeMessage`: the message is built when the job runs, as Go's hub does —
    /// the badge is read then, not when the post was written.
    Post(Box<PostPush>),
}

/// What `sendPushNotification` hands the hub for one recipient of a post.
pub(crate) struct PostPush {
    pub post: mm_model::post::Post,
    pub user: mm_model::user::User,
    pub channel: mm_model::channel::Channel,
    pub channel_name: String,
    pub sender_name: String,
    pub explicit_mention: bool,
    pub channel_wide_mention: bool,
    pub reply_to_thread_type: String,
}

/// `pushJWTClaims`: two tagged fields and an embedded `jwt.RegisteredClaims` whose seven fields
/// are all `omitempty` and all empty, so the payload is exactly these two keys in this order.
#[derive(serde::Serialize)]
struct PushJwtClaims<'a> {
    ack_id: &'a str,
    device_id: &'a str,
}

/// `jwt.NewWithClaims(jwt.SigningMethodES256, claims).SignedString(key)` (golang-jwt v5):
/// base64url of `{"alg":"ES256","typ":"JWT"}`, `.`, base64url of the claims, `.`, base64url of
/// the 64-byte `r || s`.
pub fn sign_push_jwt(
    key: &p256::ecdsa::SigningKey,
    ack_id: &str,
    device_id: &str,
) -> Result<String, serde_json::Error> {
    use base64::Engine as _;
    use p256::ecdsa::signature::Signer as _;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = engine.encode(br#"{"alg":"ES256","typ":"JWT"}"#);
    let claims = engine.encode(mm_model::utils::go_json_marshal(&PushJwtClaims {
        ack_id,
        device_id,
    })?);
    let signing_input = format!("{header}.{claims}");
    let signature: p256::ecdsa::Signature = key.sign(signing_input.as_bytes());
    Ok(format!(
        "{signing_input}.{}",
        engine.encode(signature.to_bytes())
    ))
}

impl App {
    fn enqueue(&self, job: Job) {
        let app = self.clone();
        let sema = semaphore();
        tokio::spawn(async move {
            let Ok(_permit) = sema.acquire_owned().await else {
                return;
            };
            let (kind, result) = match job {
                Job::Clear {
                    current_session_id,
                    user_id,
                    channel_id,
                    root_id,
                } => (
                    "clear",
                    app.clear_push_notification_sync(
                        &current_session_id,
                        &user_id,
                        &channel_id,
                        &root_id,
                    )
                    .await,
                ),
                Job::UpdateBadge { user_id } => (
                    "update_badge",
                    app.update_mobile_app_badge_sync(&user_id).await,
                ),
                Job::Post(push) => ("message", app.send_push_notification_sync(*push).await),
            };
            if let Err(err) = result {
                tracing::error!(notification_type = kind, error = %err.id, "Unable to send push notification");
            }
        });
    }

    /// Port of `App.clearPushNotification` (notification_push.go:406): queue a `clear` for one
    /// channel (or thread) on every device of `user_id` except the session that read it.
    pub fn clear_push_notification(
        &self,
        current_session_id: &str,
        user_id: &str,
        channel_id: &str,
        root_id: &str,
    ) {
        self.enqueue(Job::Clear {
            current_session_id: current_session_id.to_owned(),
            user_id: user_id.to_owned(),
            channel_id: channel_id.to_owned(),
            root_id: root_id.to_owned(),
        });
    }

    /// Port of `App.UpdateMobileAppBadge` (notification_push.go:436).
    pub fn update_mobile_app_badge(&self, user_id: &str) {
        self.enqueue(Job::UpdateBadge {
            user_id: user_id.to_owned(),
        });
    }

    /// Port of `App.sendPushNotification` (notification_push.go:285): the names are resolved now,
    /// with the recipient's name format, and the message is built when the hub runs the job.
    pub(crate) async fn send_push_notification(
        &self,
        notification: &crate::notification::PostNotification<'_>,
        user: &mm_model::user::User,
        explicit_mention: bool,
        channel_wide_mention: bool,
        reply_to_thread_type: &str,
    ) {
        let name_format = self.get_notification_name_format(user).await;
        let channel_name = notification.get_channel_name(&name_format, &user.id);
        let sender_name =
            notification.get_sender_name(&name_format, self.config().enable_post_username_override);
        self.enqueue(Job::Post(Box::new(PostPush {
            post: notification.post.clone(),
            user: user.clone(),
            channel: notification.channel.clone(),
            channel_name,
            sender_name,
            explicit_mention,
            channel_wide_mention,
            reply_to_thread_type: reply_to_thread_type.to_owned(),
        })));
    }

    /// Port of `App.GetNotificationNameFormat` (notification.go:1659): the username when full
    /// names are hidden, else the user's `display_settings/name_format` preference, else
    /// `TeammateNameDisplay`.
    pub async fn get_notification_name_format(&self, user: &mm_model::user::User) -> String {
        use mm_store::PreferenceStore;
        if !self.config().show_full_name {
            return mm_model::user::external::SHOW_USERNAME.to_owned();
        }
        match self
            .store()
            .preference()
            .get(&user.id, "display_settings", "name_format")
            .await
        {
            Ok(preference) => preference.value,
            Err(_) => crate::config::load_model_config(self.store().config())
                .await
                .ok()
                .and_then(|c| c.team_settings.teammate_name_display)
                .unwrap_or_default(),
        }
    }

    /// Port of `App.sendPushNotificationSync` (notification_push.go:70).
    async fn send_push_notification_sync(&self, push: PostPush) -> AppResult {
        let contents = crate::config::load_model_config(self.store().config())
            .await
            .ok()
            .and_then(|c| c.email_settings.push_notification_contents)
            .unwrap_or_default();
        let user_id = push.user.id.clone();
        let msg = self
            .build_push_notification_message(&contents, &push)
            .await?;
        self.send_push_notification_to_all_sessions(msg, &user_id, "")
            .await
    }

    /// Port of `App.BuildPushNotificationMessage` (notification_push.go:790).
    ///
    /// **`id_loaded` falls back to `generic`**: its content is fetched later through
    /// `App.Notification()`, the Enterprise interface, which is nil on every build we run — so Go
    /// takes the same fallback.
    pub(crate) async fn build_push_notification_message(
        &self,
        contents_config: &str,
        push: &PostPush,
    ) -> AppResult<PushNotification> {
        let contents = if contents_config == ID_LOADED_NOTIFICATION {
            GENERIC_NOTIFICATION
        } else {
            contents_config
        };
        let mut msg = self
            .build_full_push_notification_message(contents, push)
            .await;
        let is_crt = self.is_crt_enabled_for_user(&push.user.id).await;
        msg.badge = self
            .get_user_badge_count(&push.user.id, is_crt)
            .await
            .map_err(|_| {
                AppError::boxed(
                    "BuildPushNotificationMessage",
                    "app.user.get_badge_count.app_error",
                    None,
                    "",
                    500,
                )
            })?;
        msg.post_type = push.post.post_type.clone();
        msg.channel_type = push.channel.channel_type.clone();
        Ok(msg)
    }

    /// Port of `App.buildFullPushNotificationMessage` (notification_push.go:885).
    async fn build_full_push_notification_message(
        &self,
        contents_config: &str,
        push: &PostPush,
    ) -> PushNotification {
        let post = &push.post;
        let channel = &push.channel;
        let mut msg = PushNotification {
            category: mm_model::push_notification::CATEGORY_CAN_REPLY.to_owned(),
            version: PUSH_MESSAGE_V2.to_owned(),
            type_: PUSH_TYPE_MESSAGE.to_owned(),
            team_id: channel.team_id.clone(),
            channel_id: channel.id.clone(),
            post_id: post.id.clone(),
            root_id: post.root_id.clone(),
            sender_id: post.user_id.clone(),
            ..PushNotification::default()
        };
        let t = crate::email::user_translations(&push.user.locale)
            .await
            .ok();
        let tr = |id: &str| t.as_ref().map_or_else(|| id.to_owned(), |t| t.t(id));

        let is_dm = channel.channel_type == mm_model::channel::CHANNEL_TYPE_DIRECT;
        if contents_config != GENERIC_NO_CHANNEL_NOTIFICATION || is_dm {
            msg.channel_name = push.channel_name.clone();
        }
        if self.is_crt_enabled_for_user(&push.user.id).await {
            msg.is_crt_enabled = true;
            if !post.root_id.is_empty() && contents_config != GENERIC_NO_CHANNEL_NOTIFICATION {
                msg.channel_name = t.as_ref().map_or_else(
                    || "api.push_notification.title.collapsed_threads".to_owned(),
                    |t| {
                        t.tp(
                            "api.push_notification.title.collapsed_threads",
                            &[(
                                "channelName",
                                serde_json::Value::String(push.channel_name.clone()),
                            )],
                        )
                    },
                );
                if is_dm {
                    msg.channel_name = tr("api.push_notification.title.collapsed_threads_dm");
                }
            }
        }

        msg.sender_name = push.sender_name.clone();
        let prop = |key: &str| {
            post.get_prop(key)
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        };
        if let Some(username) = prop(mm_model::post::POST_PROPS_OVERRIDE_USERNAME)
            && self.config().enable_post_username_override
        {
            msg.override_username = username.clone();
            msg.sender_name = username;
        }
        if let Some(icon) = prop(POST_PROPS_OVERRIDE_ICON_URL)
            && self.config().enable_post_icon_override
        {
            msg.override_icon_url = icon;
        }
        if let Some(from_webhook) = prop(mm_model::post::POST_PROPS_FROM_WEBHOOK) {
            msg.from_webhook = from_webhook;
        }

        let mut post_message = match crate::markdown_utils::strip_markdown_and_decode(&post.message)
        {
            Ok(stripped) => stripped,
            Err(err) => {
                tracing::warn!(post_id = %post.id, error = %err, "Failed to strip markdown from post");
                post.message.clone()
            }
        };
        for attachment in post.attachments() {
            if !attachment.fallback.is_empty() {
                post_message.push('\n');
                post_message.push_str(&attachment.fallback);
            }
        }
        let has_files = post.file_ids.as_ref().is_some_and(|ids| !ids.is_empty());

        msg.message = get_push_notification_message(
            contents_config,
            &post_message,
            push.explicit_mention,
            push.channel_wide_mention,
            has_files,
            &msg.sender_name,
            &channel.channel_type,
            &push.reply_to_thread_type,
            &tr,
        );
        msg
    }

    /// Port of `App.ShouldSendPushNotification` (notification_push.go:669).
    pub(crate) fn should_send_push_notification(
        user: &mm_model::user::User,
        channel_notify_props: Option<&mm_model::utils::StringMap>,
        was_mentioned: bool,
        status: &mm_model::status::Status,
        post: &mm_model::post::Post,
        is_gm: bool,
    ) -> bool {
        if user.is_bot {
            return false;
        }
        // `prop != nil && prop != ""` — any non-empty string forces, and so does any non-string.
        if let Some(prop) = post.get_prop(POST_PROPS_FORCE_NOTIFICATION)
            && !prop.is_null()
            && prop.as_str() != Some("")
        {
            return true;
        }
        if does_notify_props_allow_push_notification(
            user,
            channel_notify_props,
            post,
            was_mentioned,
            is_gm,
        )
        .is_some()
        {
            return false;
        }
        does_status_allow_push_notification(
            user.notify_props.as_ref(),
            status,
            &post.channel_id,
            false,
        )
        .is_none()
    }

    /// Port of `App.getUserBadgeCount` (notification_push.go:367): the unread mentions over the
    /// user's channels, plus the unread thread mentions under collapsed threads.
    pub async fn get_user_badge_count(
        &self,
        user_id: &str,
        is_crt_enabled: bool,
    ) -> AppResult<i64> {
        let unread = self
            .store()
            .user()
            .get_unread_count(user_id, is_crt_enabled)
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "unread count failed");
                AppError::boxed(
                    "getUserBadgeCount",
                    "app.user.get_unread_count.app_error",
                    None,
                    "",
                    500,
                )
            })?;
        if !is_crt_enabled {
            return Ok(unread);
        }
        let threads = self
            .store()
            .thread()
            .get_total_unread_mentions(
                user_id,
                "",
                &mm_model::thread::GetUserThreadsOpts::default(),
            )
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "thread mention count failed");
                AppError::boxed(
                    "getUserBadgeCount",
                    "app.user.get_thread_count_for_user.app_error",
                    None,
                    "",
                    500,
                )
            })?;
        Ok(unread + threads)
    }

    async fn badge_or_error(&self, where_: &str, user_id: &str, is_crt: bool) -> AppResult<i64> {
        self.get_user_badge_count(user_id, is_crt)
            .await
            .map_err(|_| {
                AppError::boxed(where_, "app.user.get_badge_count.app_error", None, "", 500)
            })
    }

    /// Port of `App.clearPushNotificationSync` (notification_push.go:385).
    pub async fn clear_push_notification_sync(
        &self,
        current_session_id: &str,
        user_id: &str,
        channel_id: &str,
        root_id: &str,
    ) -> AppResult {
        let is_crt_enabled = self.is_crt_enabled_for_user(user_id).await;
        let badge = self
            .badge_or_error("clearPushNotificationSync", user_id, is_crt_enabled)
            .await?;
        let msg = PushNotification {
            type_: PUSH_TYPE_CLEAR.to_owned(),
            version: PUSH_MESSAGE_V2.to_owned(),
            channel_id: channel_id.to_owned(),
            root_id: root_id.to_owned(),
            content_available: 1,
            badge,
            is_crt_enabled,
            ..PushNotification::default()
        };
        self.send_push_notification_to_all_sessions(msg, user_id, current_session_id)
            .await
    }

    /// Port of `App.updateMobileAppBadgeSync` (notification_push.go:420). Unlike the clear, it
    /// sets `sound: "none"` and does **not** carry `is_crt_enabled`.
    pub async fn update_mobile_app_badge_sync(&self, user_id: &str) -> AppResult {
        let is_crt_enabled = self.is_crt_enabled_for_user(user_id).await;
        let badge = self
            .badge_or_error("updateMobileAppBadgeSync", user_id, is_crt_enabled)
            .await?;
        let msg = PushNotification {
            type_: PUSH_TYPE_UPDATE_BADGE.to_owned(),
            version: PUSH_MESSAGE_V2.to_owned(),
            sound: "none".to_owned(),
            content_available: 1,
            badge,
            ..PushNotification::default()
        };
        self.send_push_notification_to_all_sessions(msg, user_id, "")
            .await
    }

    /// Port of `App.sendPushNotificationToAllSessions` (notification_push.go:93).
    ///
    /// # Which token a session is sent to
    ///
    /// A token is *usable* when it is set and is not the one the proxy last reported removed. A
    /// `voip` message goes to the VoIP token, or is **downgraded** to a standard message on the
    /// standard token when only that one is usable; any other message goes to the standard token
    /// only, never falling back to VoIP ("silence chat, keep ringing"). A session with neither is
    /// skipped, as is an expired one and the session the caller asked to skip.
    ///
    /// # Plugins first
    ///
    /// `NotificationWillBePushed` runs once, before the sessions are read: a plugin may replace
    /// the message (its `transport` is restored — plugins built against an older model zero it)
    /// or reject it, which is not an error.
    pub async fn send_push_notification_to_all_sessions(
        &self,
        msg: PushNotification,
        user_id: &str,
        skip_session_id: &str,
    ) -> AppResult {
        let Some(msg) = self.run_notification_will_be_pushed(msg, user_id).await else {
            return Ok(());
        };

        let sessions = self
            .store()
            .session()
            .get_sessions_with_active_device_ids(user_id)
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "mobile sessions lookup failed");
                AppError::boxed(
                    "getMobileAppSessions",
                    "app.session.get_sessions.app_error",
                    None,
                    "",
                    500,
                )
            })?;

        let key = self.asymmetric_signing_key().await;
        let server_id = self.server_id().await;

        for mut session in sessions {
            if session.is_expired()
                || (!skip_session_id.is_empty() && skip_session_id == session.id)
            {
                continue;
            }
            let mut tmp = msg.deep_copy();
            let prop = |name: &str| {
                session
                    .props
                    .as_ref()
                    .and_then(|p| p.get(name))
                    .cloned()
                    .unwrap_or_default()
            };
            let standard_usable = !session.device_id.is_empty()
                && prop(PROP_LAST_REMOVED_DEVICE_ID) != session.device_id;
            let voip_usable = !session.voip_device_id.is_empty()
                && prop(PROP_LAST_REMOVED_VOIP_DEVICE_ID) != session.voip_device_id;

            let device_id = if tmp.transport.as_str() == PushTransport::VOIP {
                if voip_usable {
                    session.voip_device_id.clone()
                } else if standard_usable {
                    tmp.transport = PushTransport(PushTransport::STANDARD.to_owned());
                    session.device_id.clone()
                } else {
                    continue;
                }
            } else if standard_usable {
                session.device_id.clone()
            } else {
                continue;
            };
            tmp.set_device_id_and_platform(&device_id);
            tmp.ack_id = mm_model::utils::new_id();

            // `SignedString(nil)` is golang-jwt's "key is invalid": logged, and this session
            // skipped.
            let Some(key) = key.as_ref() else {
                tracing::error!(ack_id = %tmp.ack_id, "Notification error: key is invalid");
                continue;
            };
            match sign_push_jwt(key, &tmp.ack_id, &tmp.device_id) {
                Ok(signature) => tmp.signature = signature,
                Err(err) => {
                    tracing::error!(ack_id = %tmp.ack_id, error = %err, "Notification error");
                    continue;
                }
            }

            if let Err(err) = self
                .send_to_push_proxy(&mut tmp, &mut session, &server_id)
                .await
            {
                tracing::error!(
                    ack_id = %tmp.ack_id,
                    push_type = %tmp.type_,
                    session_id = %session.id,
                    error = %err,
                    "Failed to send to push proxy"
                );
                continue;
            }
            tracing::trace!(ack_id = %tmp.ack_id, "Notification sent to push proxy");
        }
        Ok(())
    }

    /// `RunMultiHook(NotificationWillBePushed)` — `None` when a plugin rejected the message.
    async fn run_notification_will_be_pushed(
        &self,
        msg: PushNotification,
        user_id: &str,
    ) -> Option<PushNotification> {
        let Some(environment) = self.hook_environment() else {
            return Some(msg);
        };
        let original_transport = msg.transport.clone();
        let mut msg = msg;
        for (hooks, manifest) in
            environment.hooks_implementing(mm_plugin::rpc::hook_id::NOTIFICATION_WILL_BE_PUSHED)
        {
            let returns = hooks
                .notification_will_be_pushed(
                    mm_plugin::wire::plugin::Z_NotificationWillBePushedArgs {
                        a: Some(Box::new(crate::plugin_hooks::push_notification_to_wire(
                            &msg,
                        ))),
                        b: user_id.to_owned(),
                    },
                )
                .await;
            if !returns.b.is_empty() {
                tracing::info!(
                    rejection_reason = %returns.b,
                    plugin_id = %manifest.id,
                    user_id = %user_id,
                    "Notification cancelled by plugin."
                );
                return None;
            }
            if let Some(replacement) = returns.a.as_deref() {
                msg = crate::plugin_hooks::push_notification_from_wire(replacement);
                if msg.transport != original_transport {
                    msg.transport = original_transport.clone();
                }
                tracing::info!(plugin_id = %manifest.id, "Notification modified by plugin.");
            }
        }
        Some(msg)
    }

    /// Port of `App.rawSendToPushProxy` (notification_push.go:539).
    async fn raw_send_to_push_proxy(
        &self,
        msg: &PushNotification,
    ) -> Result<std::collections::HashMap<String, String>, PushError> {
        let body = mm_model::utils::go_json_marshal(msg)?;
        let config = crate::config::load_model_config(self.store().config())
            .await
            .map_err(|err| PushError::Transport(err.to_string()))?;
        let server = config
            .email_settings
            .push_notification_server
            .as_deref()
            .unwrap_or_default();
        let url = format!(
            "{}{API_URL_SUFFIX_V1}/send_push",
            server.trim_end_matches('/')
        );
        let response = client()
            .post(&url)
            .body(body)
            .send()
            .await
            .map_err(|err| PushError::Transport(go_client_error("POST", &url, &err)))?;
        if response.status().as_u16() != 200 {
            return Err(PushError::Status(response.status().as_u16()));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|err| PushError::Decode(err.to_string()))?;
        // `json.NewDecoder(resp.Body).Decode(&pushResponse)` into a `map[string]string`
        // (notification_push.go:570): one value, `null` a nil map, a `null` member `""`.
        mm_model::utils::decode_one_value_from_json::<std::collections::HashMap<String, String>>(
            &bytes,
        )
        .map_err(|err| PushError::Decode(err.to_string()))
    }

    /// Port of `App.sendToPushProxy` (notification_push.go:577).
    ///
    /// A `remove` answer records the token as dead in the session's props — under the VoIP key
    /// for a VoIP message, so the other token is untouched — and is itself an error, which the
    /// caller logs. A `fail` answer is an error carrying the proxy's message.
    async fn send_to_push_proxy(
        &self,
        msg: &mut PushNotification,
        session: &mut Session,
        server_id: &str,
    ) -> Result<(), PushError> {
        msg.server_id = server_id.to_owned();
        let response = self.raw_send_to_push_proxy(msg).await?;
        match response.get(PUSH_STATUS).map(String::as_str) {
            Some(PUSH_STATUS_REMOVE) => {
                let (prop, value) = if msg.transport.as_str() == PushTransport::VOIP {
                    (
                        PROP_LAST_REMOVED_VOIP_DEVICE_ID,
                        session.voip_device_id.clone(),
                    )
                } else {
                    (PROP_LAST_REMOVED_DEVICE_ID, session.device_id.clone())
                };
                self.set_extra_session_props(session, &[(prop, value.as_str())])
                    .await
                    .map_err(|err| PushError::SessionProps(err.id.clone()))?;
                self.clear_session_cache_for_user(&session.user_id).await;
                Err(PushError::RemoveDevice)
            }
            Some(PUSH_STATUS_FAIL) => Err(PushError::Proxy(
                response
                    .get(PUSH_STATUS_ERROR_MSG)
                    .cloned()
                    .unwrap_or_default(),
            )),
            _ => Ok(()),
        }
    }

    /// Port of `App.SendAckToPushProxy` (notification_push.go:614): the ack, re-encoded, posted
    /// to `/api/v1/ack`; the body of a 200 is read and discarded.
    pub async fn send_ack_to_push_proxy(&self, ack: &PushNotificationAck) -> Result<(), PushError> {
        let body = mm_model::utils::go_json_marshal(ack)?;
        let config = crate::config::load_model_config(self.store().config())
            .await
            .map_err(|err| PushError::Transport(err.to_string()))?;
        let server = config
            .email_settings
            .push_notification_server
            .as_deref()
            .unwrap_or_default();
        let url = format!("{}{API_URL_SUFFIX_V1}/ack", server.trim_end_matches('/'));
        let response = client().post(&url).body(body).send().await.map_err(|err| {
            PushError::Transport(format!(
                "failed to send: {}",
                go_client_error("POST", &url, &err)
            ))
        })?;
        if response.status().as_u16() != 200 {
            return Err(PushError::Status(response.status().as_u16()));
        }
        let _ = response.bytes().await;
        Ok(())
    }

    /// Port of `App.canSendPushNotifications` (app/notification.go:23): on, and — for one of
    /// Mattermost's hosted proxies (MHPNS) — licensed for it.
    pub async fn can_send_push_notifications(&self) -> bool {
        let Ok(config) = crate::config::load_model_config(self.store().config()).await else {
            return false;
        };
        if !config
            .email_settings
            .send_push_notifications
            .unwrap_or(false)
        {
            return false;
        }
        let server = config
            .email_settings
            .push_notification_server
            .as_deref()
            .unwrap_or_default();
        use mm_model::push_notification::{
            MHPNS, MHPNS_AP, MHPNS_EU, MHPNS_GLOBAL, MHPNS_LEGACY_DE, MHPNS_LEGACY_US, MHPNS_US,
        };
        let is_mhpns = [
            MHPNS,
            MHPNS_LEGACY_US,
            MHPNS_LEGACY_DE,
            MHPNS_GLOBAL,
            MHPNS_US,
            MHPNS_EU,
            MHPNS_AP,
        ]
        .contains(&server);
        if is_mhpns {
            let licensed = self
                .license()
                .await
                .ok()
                .flatten()
                .is_some_and(|l| l.features.as_ref().and_then(|f| f.mhpns).unwrap_or(false));
            if !licensed {
                tracing::warn!(
                    "Push notifications have been disabled. Update your license or go to System Console > Environment > Push Notification Server to use a different server"
                );
                return false;
            }
        }
        true
    }

    /// Port of `App.SendTestPushNotification` (notification_push.go:820): `"true"`, `"false"` or
    /// `"unknown"` — the string the attach-device route puts in its response.
    pub async fn send_test_push_notification(&self, device_id: &str) -> &'static str {
        if !self.can_send_push_notifications().await {
            return "false";
        }
        let mut msg = PushNotification {
            version: "2".to_owned(),
            type_: PUSH_TYPE_TEST.to_owned(),
            server_id: self.server_id().await,
            badge: -1,
            ..PushNotification::default()
        };
        msg.set_device_id_and_platform(device_id);
        match self.raw_send_to_push_proxy(&msg).await {
            Err(err) => {
                tracing::error!(error = %err, "Failed to send test notification to push proxy");
                "unknown"
            }
            Ok(response) => match response.get(PUSH_STATUS).map(String::as_str) {
                Some(PUSH_STATUS_REMOVE) => "false",
                Some(PUSH_STATUS_FAIL) => "unknown",
                _ => "true",
            },
        }
    }

    /// `msg.Type == model.PushTypeMessage` — for the ack route's counting branch.
    pub fn is_message_push(kind: &str) -> bool {
        kind == PUSH_TYPE_MESSAGE
    }
}

/// `model.FullNotification` and its siblings (config.go:73).
const FULL_NOTIFICATION: &str = "full";
const GENERIC_NOTIFICATION: &str = "generic";
const GENERIC_NO_CHANNEL_NOTIFICATION: &str = "generic_no_channel";
const ID_LOADED_NOTIFICATION: &str = "id_loaded";
/// `model.PostPropsForceNotification`, `model.PostPropsOverrideIconURL`.
const POST_PROPS_FORCE_NOTIFICATION: &str = "force_notification";
const POST_PROPS_OVERRIDE_ICON_URL: &str = "override_icon_url";
/// `model.StatusChannelTimeout` — 20 seconds.
const STATUS_CHANNEL_TIMEOUT: i64 = 20_000;

/// Port of `getPushNotificationMessage` (notification_push.go:312): the notification's text,
/// by the contents setting, the channel type and why the user is being told.
#[allow(clippy::too_many_arguments)]
pub(crate) fn get_push_notification_message(
    contents_config: &str,
    post_message: &str,
    explicit_mention: bool,
    channel_wide_mention: bool,
    has_files: bool,
    sender_name: &str,
    channel_type: &str,
    reply_to_thread_type: &str,
    t: &dyn Fn(&str) -> String,
) -> String {
    use mm_model::channel::CHANNEL_TYPE_DIRECT;
    use mm_model::user::{COMMENTS_NOTIFY_ANY, COMMENTS_NOTIFY_ROOT, USER_NOTIFY_ALL};
    const COMMENTS_NOTIFY_CRT: &str = "crt";
    let is_dm = channel_type == CHANNEL_TYPE_DIRECT;
    if post_message.is_empty() && has_files {
        let image_only = t("api.post.send_notifications_and_forget.push_image_only");
        if is_dm {
            return image_only.trim_matches(' ').to_owned();
        }
        return format!("{sender_name}{image_only}");
    }
    if contents_config == FULL_NOTIFICATION {
        if is_dm && reply_to_thread_type != COMMENTS_NOTIFY_CRT {
            return mm_model::utils::clear_mention_tags(post_message);
        }
        return format!(
            "{sender_name}: {}",
            mm_model::utils::clear_mention_tags(post_message)
        );
    }
    if is_dm {
        if reply_to_thread_type == COMMENTS_NOTIFY_CRT {
            if contents_config == GENERIC_NO_CHANNEL_NOTIFICATION {
                return format!(
                    "{sender_name}{}",
                    t("api.post.send_notification_and_forget.push_comment_on_crt_thread")
                );
            }
            return format!(
                "{sender_name}{}",
                t("api.post.send_notification_and_forget.push_comment_on_crt_thread_dm")
            );
        }
        return t("api.post.send_notifications_and_forget.push_message");
    }
    let suffix = if reply_to_thread_type == COMMENTS_NOTIFY_CRT {
        "api.post.send_notification_and_forget.push_comment_on_crt_thread"
    } else if channel_wide_mention {
        "api.post.send_notification_and_forget.push_channel_mention"
    } else if explicit_mention {
        "api.post.send_notifications_and_forget.push_explicit_mention"
    } else if reply_to_thread_type == COMMENTS_NOTIFY_ROOT {
        "api.post.send_notification_and_forget.push_comment_on_post"
    } else if reply_to_thread_type == COMMENTS_NOTIFY_ANY {
        "api.post.send_notification_and_forget.push_comment_on_thread"
    } else if reply_to_thread_type == USER_NOTIFY_ALL {
        "api.post.send_notification_and_forget.push_comment_on_crt_thread"
    } else {
        "api.post.send_notifications_and_forget.push_general_message"
    };
    format!("{sender_name}{}", t(suffix))
}

/// Port of `doesNotifyPropsAllowPushNotification` (notification_push.go:717): the reason not to
/// push, or `None`.
///
/// The final `notify == all` branch returns `""` whichever way its condition goes, so an `all`
/// level never refuses — **not even the author's own post**. Go's comment-free dead branch,
/// reproduced as the absence of a refusal.
pub(crate) fn does_notify_props_allow_push_notification(
    user: &mm_model::user::User,
    channel_notify_props: Option<&mm_model::utils::StringMap>,
    post: &mm_model::post::Post,
    was_mentioned: bool,
    is_gm: bool,
) -> Option<&'static str> {
    use mm_model::channel_member::{
        CHANNEL_NOTIFY_ALL, CHANNEL_NOTIFY_DEFAULT, CHANNEL_NOTIFY_MENTION, CHANNEL_NOTIFY_NONE,
    };
    let user_notify = user
        .notify_props
        .as_ref()
        .and_then(|p| p.get(mm_model::user::PUSH_NOTIFY_PROP))
        .map_or("", String::as_str);
    let channel_prop = |key: &str| {
        channel_notify_props
            .and_then(|p| p.get(key))
            .map(String::as_str)
    };
    let channel_notify = match channel_prop(mm_model::user::PUSH_NOTIFY_PROP) {
        Some(value) if !value.is_empty() => value,
        _ => CHANNEL_NOTIFY_DEFAULT,
    };
    let mut notify = channel_notify;
    if channel_notify == CHANNEL_NOTIFY_DEFAULT {
        notify = user_notify;
        if is_gm && user_notify == mm_model::user::USER_NOTIFY_MENTION {
            notify = CHANNEL_NOTIFY_ALL;
        }
    }
    if channel_prop("mark_unread") == Some("mention") {
        return Some("channel_muted");
    }
    if post.has_silent_notification() && !post.has_force_notification() {
        return Some("silent");
    }
    if post.is_system_message() {
        return Some("system_message");
    }
    if notify == CHANNEL_NOTIFY_NONE {
        return Some("level_set_to_none");
    }
    if notify == CHANNEL_NOTIFY_MENTION && !was_mentioned {
        return Some("not_mentioned");
    }
    None
}

/// Port of `doesStatusAllowPushNotification` (notification_push.go:764).
///
/// A missing `push_status` prop behaves like `online` — "always push" — and so does being in a
/// different channel, idle for 20 seconds, or under collapsed threads.
pub(crate) fn does_status_allow_push_notification(
    user_notify_props: Option<&mm_model::utils::StringMap>,
    status: &mm_model::status::Status,
    channel_id: &str,
    is_crt: bool,
) -> Option<&'static str> {
    use mm_model::status::{STATUS_AWAY, STATUS_OFFLINE, STATUS_ONLINE};
    if status.status == "dnd" || status.status == "ooo" {
        return Some("user_status");
    }
    let push_status = user_notify_props.and_then(|p| p.get("push_status"));
    let send_online_notification = status.active_channel != channel_id
        || mm_model::utils::get_millis() - status.last_activity_at > STATUS_CHANNEL_TIMEOUT
        || is_crt;
    let push_status_str = push_status.map(String::as_str);
    if (push_status_str == Some(STATUS_ONLINE) || push_status.is_none()) && send_online_notification
    {
        return None;
    }
    if push_status_str == Some(STATUS_AWAY)
        && (status.status == STATUS_AWAY || status.status == STATUS_OFFLINE)
    {
        return None;
    }
    if push_status_str == Some(STATUS_OFFLINE) && status.status == STATUS_OFFLINE {
        return None;
    }
    Some("user_is_active")
}

/// `(*url.Error).Error()` for a failed `client.Do`: `Post "<url>": <cause>`. The cause is
/// reqwest's words, not Go's `net` package's — only the log sees it.
fn go_client_error(method: &str, url: &str, err: &reqwest::Error) -> String {
    let method = method[..1].to_uppercase() + &method[1..].to_lowercase();
    format!("{method} \"{url}\": {err}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The token golang-jwt would produce, checked the way the push proxy checks it: the header
    /// and claims decode to Go's exact bytes, and the signature verifies under the public key.
    #[test]
    fn the_ack_jwt_is_es256_over_the_two_claims() {
        use base64::Engine as _;
        use p256::ecdsa::signature::Verifier as _;
        let key = p256::ecdsa::SigningKey::from_slice(&[7u8; 32]).expect("a key");
        let token = sign_push_jwt(&key, "ack<1>", "apple_rn:dev").expect("signs");
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        assert_eq!(
            engine.decode(parts[0]).expect("b64"),
            br#"{"alg":"ES256","typ":"JWT"}"#
        );
        // `json.Marshal` escapes `<` and `>` — the claims are Go's encoder's bytes.
        assert_eq!(
            engine.decode(parts[1]).expect("b64"),
            br#"{"ack_id":"ack\u003c1\u003e","device_id":"apple_rn:dev"}"#
        );
        let signature = p256::ecdsa::Signature::from_slice(&engine.decode(parts[2]).expect("b64"))
            .expect("r || s");
        key.verifying_key()
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .expect("verifies");
    }
}
