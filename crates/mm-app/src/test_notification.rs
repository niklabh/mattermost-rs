//! Port of `App.SendTestMessage` (app/post.go:3422), the body of `POST /api/v4/notifications/test`.

use mm_model::post::{POST_TYPE_DEFAULT, Post};
use mm_model::session::Session;
use mm_model::utils::{AppError, AppResult};

use crate::App;
use crate::channel_create::ChannelCreate;
use crate::post::PrepareError;
use crate::post_create::CreatePostFlags;

/// `app.notifications.send_test_message.message_body` in every locale `i18n.GetUserTranslations`
/// can load that translates it, transcribed from `server/i18n/<locale>.json`.
///
/// Every other locale — one of the ten supported locales without a translation, an unsupported
/// locale, or the empty string — gets English: `GetUserTranslations` maps a locale it did not
/// load to `en`, and `tfuncWithFallback` falls back to `en` when the id comes back untranslated.
/// `test_message_matches_the_go_bundle` checks this table against the files, both ways.
const MESSAGE_BODY: &[(&str, &str)] = &[
    (
        "de",
        "Wenn du diese Testbenachrichtigung erhalten hast, hat es funktioniert!",
    ),
    (
        "en-AU",
        "If you received this test notification, it worked!",
    ),
    (
        "nl",
        "Als je deze testmelding hebt ontvangen, dan werkt het!",
    ),
    (
        "pl",
        "Jeśli otrzymałeś to powiadomienie testowe, to zadziałało!",
    ),
    (
        "pt-BR",
        "Se você recebeu essa notificação de teste, ela funcionou!",
    ),
    ("sv", "Om du fick detta testmeddelande så fungerar det!"),
    ("tr", "Bu deneme bildirimini aldıysanız, çalışıyor!"),
    (
        "uk",
        "Якщо ви отримали це тестове сповіщення, значить він працює!",
    ),
    ("ko", "이 테스트 알림을 받으셨다면 성공하신 것입니다!"),
    ("zh-CN", "如果您收到了这条测试通知，表示通知正常工作！"),
    (
        "ja",
        "このテスト通知を受け取れた場合、通知は動作しています!",
    ),
];

/// The English body, which is also the fallback.
const MESSAGE_BODY_EN: &str = "If you received this test notification, it worked!";

/// `T("app.notifications.send_test_message.message_body")` under
/// `i18n.GetUserTranslations(locale)`. The lookup is exact and case-sensitive, as Go's map is: the
/// table's keys are only ever reached through a supported locale.
pub fn test_message_body(locale: &str) -> &'static str {
    MESSAGE_BODY
        .iter()
        .find(|(key, _)| *key == locale)
        .map_or(MESSAGE_BODY_EN, |(_, body)| body)
}

/// What [`App::send_test_message`] did.
#[derive(Debug)]
pub enum SendTestMessage {
    /// The post is written.
    Sent(Box<Post>),
    /// Nothing is posted yet: a stage this server does not reproduce was reached. The system bot
    /// and the DM may already exist — both are get-or-create, so Go finds them.
    Forward(&'static str),
}

fn send_test_message_error(id: &str, err: AppError) -> Box<AppError> {
    Box::new(AppError::new("SendTestMessage", id, None, String::new(), 500).wrap(err))
}

impl App {
    /// Port of `App.SendTestMessage` (app/post.go:3422): the system bot, the DM between it and
    /// `session`'s user, that user's locale, then `CreatePost` with `ForceNotification` under the
    /// **caller's** session (Go passes `rctx`, so `is_oauth` and the pending-post cache are the
    /// caller's).
    ///
    /// Every failure is a 500 whose id names the stage — `no_bot`, `no_channel`, `no_user`,
    /// `create_post` — with the underlying error wrapped, so a `CreatePost` 400 or 403 still
    /// reaches the client as the 500 `create_post`.
    #[tracing::instrument(skip_all, fields(user_id = %session.user_id))]
    pub async fn send_test_message(&self, session: &Session) -> AppResult<SendTestMessage> {
        let bot = self.get_system_bot().await.map_err(|err| {
            send_test_message_error("app.notifications.send_test_message.errors.no_bot", *err)
        })?;

        let channel = match self
            .get_or_create_direct_channel(&session.user_id, &bot.user_id)
            .await
            .map_err(|err| {
                send_test_message_error(
                    "app.notifications.send_test_message.errors.no_channel",
                    *err,
                )
            })? {
            ChannelCreate::Created(channel) => channel,
            ChannelCreate::Forward(reason) => return Ok(SendTestMessage::Forward(reason)),
        };

        let user = self.get_user(&session.user_id).await.map_err(|err| {
            send_test_message_error("app.notifications.send_test_message.errors.no_user", *err)
        })?;

        let post = Post {
            channel_id: channel.id.clone(),
            message: test_message_body(&user.locale).to_owned(),
            post_type: POST_TYPE_DEFAULT.to_owned(),
            user_id: bot.user_id,
            ..Post::default()
        };

        // "We don't check the preview membership because the test message does not send a link
        // to a different post."
        match self
            .create_post(
                post,
                &channel,
                session,
                CreatePostFlags {
                    force_notification: true,
                    ..CreatePostFlags::default()
                },
            )
            .await
        {
            Ok((post, _)) => Ok(SendTestMessage::Sent(Box::new(post))),
            Err(PrepareError::Unreproducible(reason)) => Ok(SendTestMessage::Forward(reason)),
            Err(PrepareError::App(err)) => Err(send_test_message_error(
                "app.notifications.send_test_message.errors.create_post",
                *err,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table against Go's own bundle, both ways: every supported locale whose file translates
    /// the id is in the table with that text, and every other supported locale falls back to the
    /// English text.
    #[test]
    fn test_message_matches_the_go_bundle() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../reference/mattermost/server/i18n");
        if !dir.is_dir() {
            return;
        }
        for locale in crate::i18n::SUPPORTED_LOCALES {
            let raw = std::fs::read_to_string(dir.join(format!("{locale}.json"))).unwrap();
            let entries: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap();
            let go = entries
                .iter()
                .find(|e| e["id"] == "app.notifications.send_test_message.message_body")
                .and_then(|e| e["translation"].as_str())
                .filter(|t| !t.is_empty())
                .unwrap_or(MESSAGE_BODY_EN);
            assert_eq!(test_message_body(locale), go, "locale {locale}");
        }
    }

    /// No table key is outside the supported set — such an entry would be unreachable in Go.
    #[test]
    fn every_table_locale_is_supported() {
        for (locale, _) in MESSAGE_BODY {
            assert!(crate::i18n::is_supported_locale(locale), "{locale}");
        }
    }

    /// An unsupported locale, a case variant and the empty string all fall back to English.
    #[test]
    fn unknown_locales_get_english() {
        for locale in ["", "DE", "de-DE", "hi", "es"] {
            assert_eq!(test_message_body(locale), MESSAGE_BODY_EN, "{locale}");
        }
    }
}
