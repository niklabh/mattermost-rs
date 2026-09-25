//! Port of `app/expirynotify.go` — the body of the `expiry_notify` job: a push to every mobile
//! session that expired in the last hour and has not been told yet.

use mm_model::push_notification::{PUSH_MESSAGE_V2, PUSH_TYPE_SESSION, PushNotification};
use mm_model::session::Session;
use mm_model::utils::{AppError, AppResult};
use mm_store::SessionStore;

use crate::App;

/// Go's `OneHourMillis` (app/expirynotify.go:17): the window a session must have expired inside.
pub const ONE_HOUR_MILLIS: i64 = 60 * 60 * 1000;

/// `model.DefaultLocale`, the locale when the session's user cannot be read.
const DEFAULT_LOCALE: &str = "en";

impl App {
    /// Port of `App.NotifySessionsExpired` (app/expirynotify.go:20).
    ///
    /// Nothing at all unless push notifications can be sent (`canSendPushNotifications`). Then
    /// `GetSessionsExpired(OneHourMillis, mobileOnly: true, unnotifiedOnly: true)`, whose failure
    /// is the job's error (`app.session.analytics_session_count.app_error`, Go's id). Each
    /// session gets a v2 `session` push to its `DeviceId`, **unsigned** — Go's caller here does
    /// not sign, unlike the post pushes — and only a push the proxy accepted sets
    /// `ExpiredNotify`; a failed send is logged and the session is tried again next run. A failed
    /// flag write is logged too, and the loop goes on.
    #[tracing::instrument(skip(self), fields(sessions))]
    pub async fn notify_sessions_expired(&self) -> AppResult<()> {
        if !self.can_send_push_notifications().await {
            return Ok(());
        }

        let sessions = self
            .store()
            .session()
            .get_sessions_expired(ONE_HOUR_MILLIS, true, true)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "Cannot get sessions expired");
                AppError::boxed(
                    "NotifySessionsExpired",
                    "app.session.analytics_session_count.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        tracing::Span::current().record("sessions", sessions.len());

        let msg = PushNotification {
            version: PUSH_MESSAGE_V2.to_owned(),
            type_: PUSH_TYPE_SESSION.to_owned(),
            ..PushNotification::default()
        };
        let server_id = self.server_id().await;

        for mut session in sessions {
            let mut push = msg.deep_copy();
            push.set_device_id_and_platform(&session.device_id);
            push.ack_id = mm_model::utils::new_id();
            push.message = self.session_expired_push_message(&session).await;

            if let Err(err) = self
                .send_to_push_proxy(&mut push, &mut session, &server_id)
                .await
            {
                tracing::error!(
                    ack_id = %push.ack_id,
                    session_id = %session.id,
                    user_id = %session.user_id,
                    error = %err,
                    "Failed to send to push proxy"
                );
                continue;
            }

            if let Err(err) = self
                .store()
                .session()
                .update_expired_notify(&session.id, true)
                .await
            {
                tracing::error!(session_id = %session.id, error = %err, "Failed to update ExpiredNotify flag");
            }
        }
        Ok(())
    }

    /// Port of `App.getSessionExpiredPushMessage` (app/expirynotify.go:94):
    /// `api.push_notifications.session.expired` in the user's locale (`en` when the user cannot
    /// be read), with `TeamSettings.SiteName` and `ServiceSettings.SessionLengthMobileInHours`.
    async fn session_expired_push_message(&self, session: &Session) -> String {
        const ID: &str = "api.push_notifications.session.expired";
        let locale = match self.get_user(&session.user_id).await {
            Ok(user) => user.locale,
            Err(_) => DEFAULT_LOCALE.to_owned(),
        };
        let (site_name, hours) = match crate::config::load_model_config(self.store().config()).await
        {
            Ok(config) => (
                config.team_settings.site_name.unwrap_or_default(),
                config
                    .service_settings
                    .session_length_mobile_in_hours
                    .unwrap_or(self.config().session_length_mobile_in_hours),
            ),
            Err(err) => {
                tracing::error!(error = %err, "the config could not be read for the push message");
                (String::new(), self.config().session_length_mobile_in_hours)
            }
        };
        let params = crate::i18n::Params::from([
            ("siteName".to_owned(), serde_json::Value::String(site_name)),
            ("hoursCount".to_owned(), serde_json::Value::from(hours)),
        ]);
        match crate::i18n::translations().await {
            Some(bundle) => bundle.translate_with(&locale, ID, Some(&params)),
            None => ID.to_owned(),
        }
    }
}
