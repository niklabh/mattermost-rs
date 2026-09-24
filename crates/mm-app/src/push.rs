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
    Message(Box<PushNotification>, String),
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
                Job::Message(msg, user_id) => (
                    "message",
                    app.send_push_notification_to_all_sessions(*msg, &user_id, "")
                        .await,
                ),
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

    /// Queue an already-built `message` for all of a user's devices — the hub's
    /// `notificationTypeMessage` arm after `BuildPushNotificationMessage`.
    pub(crate) fn queue_push_message(&self, msg: PushNotification, user_id: &str) {
        self.enqueue(Job::Message(Box::new(msg), user_id.to_owned()));
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
        serde_json::from_slice::<std::collections::HashMap<String, String>>(&bytes)
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
            br#"{"ack_id":"ack<1>","device_id":"apple_rn:dev"}"#
        );
        let signature = p256::ecdsa::Signature::from_slice(&engine.decode(parts[2]).expect("b64"))
            .expect("r || s");
        key.verifying_key()
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .expect("verifies");
    }
}
