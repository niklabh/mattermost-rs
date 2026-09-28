//! The two personal-access-token expiry jobs' app halves: `cleanupExpired` with
//! `App.NotifyExpiredAccessTokensDeleted` (jobs/cleanup_expired_access_tokens/worker.go,
//! app/expired_access_token_notify.go) and `App.NotifyExpiringAccessTokens`
//! (app/notify_expiring_access_tokens.go).
//!
//! # Both talk to the owner through a DM from the system bot, and a job cannot forward
//!
//! Each message is `CreatePost` in the owner's DM with the system bot, under Go's
//! `request.EmptyContext` — no session. A request this server cannot serve is handed to Go; a job
//! has no request to hand, so the two places this port answers "forward" — a DM create that needs
//! Go's plugin directory, and a `CreatePost` shape it does not reproduce — are logged and the
//! owner is skipped. Neither is reachable for a default-type post in a DM with the system bot on
//! a server without plugin bots (see `App::get_or_create_direct_channel` and
//! `App::refuse_create_post_shapes`); the warnings exist so that it would be visible if one were.

use std::collections::BTreeSet;

use mm_model::bot::Bot;
use mm_model::post::{POST_TYPE_DEFAULT, Post};
use mm_model::session::Session;
use mm_model::user_access_token::UserAccessToken;
use mm_model::utils::{AppError, AppResult};
use mm_store::UserAccessTokenStore;

use crate::App;
use crate::channel_create::ChannelCreate;
use crate::post::PrepareError;
use crate::post_create::CreatePostFlags;

/// `batchLimit` (cleanup_expired_access_tokens/worker.go:17).
pub const CLEANUP_BATCH_LIMIT: i64 = 1000;
/// `maxBatches` (cleanup_expired_access_tokens/worker.go:22).
pub const CLEANUP_MAX_BATCHES: usize = 1000;
/// `expiringAccessTokenBatchLimit` (app/notify_expiring_access_tokens.go:24).
pub const EXPIRING_BATCH_LIMIT: i64 = 1000;
/// `expiringAccessTokenThresholds` (app/notify_expiring_access_tokens.go:28): the 7/3/1-day
/// cascade, most urgent first.
pub const EXPIRING_THRESHOLDS_DAYS: [i64; 3] = [1, 3, 7];

/// The most urgent threshold a token with this much time left falls in — Go's
/// `accessTokenExpiryBucket` (app/notify_expiring_access_tokens.go:101). `0` when it has already
/// expired or is further out than the largest threshold.
#[must_use]
pub fn access_token_expiry_bucket(expires_at: i64, now: i64) -> i64 {
    let remaining = expires_at - now;
    if remaining <= 0 {
        return 0;
    }
    EXPIRING_THRESHOLDS_DAYS
        .iter()
        .copied()
        .find(|days| remaining <= days * mm_model::license::DAY_IN_MILLISECONDS)
        .unwrap_or(0)
}

/// Go's per-token skip in `NotifyExpiringAccessTokens`: a warning already sent while no more time
/// remained than the bucket spans (`LastNotifiedAt >= ExpiresAt - bucket days`) covers it.
#[must_use]
pub fn already_warned_at(token: &UserAccessToken, bucket: i64) -> bool {
    token
        .last_notified_at
        .is_some_and(|at| at >= token.expires_at - bucket * mm_model::license::DAY_IN_MILLISECONDS)
}

impl App {
    /// Port of `cleanupExpired` (jobs/cleanup_expired_access_tokens/worker.go:79) with its two
    /// callbacks: batches of `GetExpiredBefore(cutoff, limit)`, each **notified before it is
    /// deleted** (the notice needs the token-to-owner mapping), then `DeleteByIds`, then each
    /// affected user's session cache cleared. A batch shorter than `limit` or an empty one ends
    /// the run; so does `max_batches`, draining a larger backlog across runs. A store failure is
    /// the job's error; a notice failure is only logged.
    #[tracing::instrument(skip(self), fields(deleted))]
    pub async fn cleanup_expired_access_tokens(
        &self,
        cutoff: i64,
        limit: i64,
        max_batches: usize,
    ) -> Result<(), mm_store::StoreError> {
        let mut total: i64 = 0;
        for _ in 0..max_batches {
            let expired = self
                .store()
                .user_access_token()
                .get_expired_before(cutoff, limit)
                .await?;
            if expired.is_empty() {
                break;
            }
            let ids: Vec<String> = expired.iter().map(|t| t.id.clone()).collect();
            let users: BTreeSet<&str> = expired.iter().map(|t| t.user_id.as_str()).collect();

            self.notify_expired_access_tokens_deleted(&expired).await;

            total += self.store().user_access_token().delete_by_ids(&ids).await?;
            for user_id in users {
                self.clear_session_cache_for_user(user_id).await;
            }
            if i64::try_from(expired.len()).unwrap_or(i64::MAX) < limit {
                break;
            }
        }
        tracing::Span::current().record("deleted", total);
        tracing::info!(
            deleted = total,
            cutoff,
            "Cleaned up expired personal access tokens"
        );
        Ok(())
    }

    /// Port of `App.NotifyExpiredAccessTokensDeleted` (app/expired_access_token_notify.go:26):
    /// best effort. No system bot is a logged error and no notices; each owner who cannot be
    /// read, is a bot or is deactivated is skipped; the notice is
    /// `app.user_access_token.expired_deleted_notification` in the owner's locale, posted with
    /// `SetOnline: true`.
    pub async fn notify_expired_access_tokens_deleted(&self, tokens: &[UserAccessToken]) {
        if tokens.is_empty() {
            return;
        }
        let bot = match self.get_system_bot().await {
            Ok(bot) => bot,
            Err(err) => {
                tracing::error!(error = %err, "Failed to get system bot to notify expired personal access token owners");
                return;
            }
        };
        for token in tokens {
            let user = match self.get_user(&token.user_id).await {
                Ok(user) => user,
                Err(err) => {
                    tracing::warn!(user_id = %token.user_id, error = %err, "Failed to get user for expired personal access token notification");
                    continue;
                }
            };
            if user.is_bot || user.delete_at != 0 {
                continue;
            }
            let params = crate::i18n::Params::from([(
                "Description".to_owned(),
                serde_json::Value::String(token.description.clone()),
            )]);
            let message = translate(
                &user.locale,
                "app.user_access_token.expired_deleted_notification",
                Some(&params),
            )
            .await;
            if let Err(err) = self
                .dm_from_system_bot(&bot, &token.user_id, message, true)
                .await
            {
                tracing::warn!(user_id = %token.user_id, error = %err, "Failed to send expired personal access token notification");
            }
        }
    }

    /// Port of `App.NotifyExpiringAccessTokens` (app/notify_expiring_access_tokens.go:38).
    ///
    /// Off with `EnableUserAccessTokens`; otherwise `GetExpiringTokens(now, 1/3/7, 1000)` — its
    /// failure, and a missing system bot when there is anything to send, are the job's error.
    /// Each token is re-bucketed and re-checked against `LastNotifiedAt` (the store already
    /// filtered, "as a guard against races"), warned with `dm_final` for the one-day bucket or
    /// `dm` naming the days otherwise — an empty description is `unnamed_token` — posted with
    /// `SetOnline: false`, and only then stamped `LastNotifiedAt = now`. A failed warning is
    /// logged and the token is not stamped, so the next run tries again.
    #[tracing::instrument(skip(self), fields(tokens))]
    pub async fn notify_expiring_access_tokens(&self) -> AppResult<()> {
        if !self.config().enable_user_access_tokens {
            return Ok(());
        }
        let now = mm_model::utils::get_millis();
        let tokens = self
            .store()
            .user_access_token()
            .get_expiring_tokens(now, &EXPIRING_THRESHOLDS_DAYS, EXPIRING_BATCH_LIMIT)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "reading the expiring tokens failed");
                AppError::boxed(
                    "NotifyExpiringAccessTokens",
                    "app.user_access_token.get_expiring.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        tracing::Span::current().record("tokens", tokens.len());
        if tokens.is_empty() {
            return Ok(());
        }
        let bot = self.get_system_bot().await?;

        for token in &tokens {
            let bucket = access_token_expiry_bucket(token.expires_at, now);
            if bucket == 0 || already_warned_at(token, bucket) {
                continue;
            }
            if let Err(err) = self
                .send_access_token_expiry_notification(&bot, token, bucket)
                .await
            {
                tracing::error!(token_id = %token.id, user_id = %token.user_id, error = %err, "Failed to send personal access token expiry notification");
                continue;
            }
            if let Err(err) = self
                .store()
                .user_access_token()
                .update_last_notified_at(&token.id, now)
                .await
            {
                tracing::error!(token_id = %token.id, error = %err, "Failed to update LastNotifiedAt for personal access token");
            }
        }
        Ok(())
    }

    /// Port of `App.sendAccessTokenExpiryNotification` (app/notify_expiring_access_tokens.go:114),
    /// in its order: the DM channel, then the owner (whose failure is this token's error), then
    /// the message.
    async fn send_access_token_expiry_notification(
        &self,
        bot: &Bot,
        token: &UserAccessToken,
        bucket: i64,
    ) -> AppResult<()> {
        let channel = match self.system_bot_dm(bot, &token.user_id).await? {
            Some(channel) => channel,
            None => return Ok(()),
        };
        let user = self.get_user(&token.user_id).await?;
        let description = if token.description.is_empty() {
            translate(
                &user.locale,
                "app.notify_expiring_access_tokens.unnamed_token",
                None,
            )
            .await
        } else {
            token.description.clone()
        };
        let message = if bucket <= 1 {
            let params = crate::i18n::Params::from([(
                "Description".to_owned(),
                serde_json::Value::String(description),
            )]);
            translate(
                &user.locale,
                "app.notify_expiring_access_tokens.dm_final",
                Some(&params),
            )
            .await
        } else {
            let params = crate::i18n::Params::from([
                (
                    "Description".to_owned(),
                    serde_json::Value::String(description),
                ),
                ("Days".to_owned(), serde_json::Value::from(bucket)),
            ]);
            translate(
                &user.locale,
                "app.notify_expiring_access_tokens.dm",
                Some(&params),
            )
            .await
        };
        self.post_as_system_bot(bot, &channel, message, false).await
    }

    /// `GetOrCreateDirectChannel(rctx, owner, systemBot)` with no session, then `CreatePost` of
    /// a default-type post by the bot. See the module note for the two "forward" answers.
    async fn dm_from_system_bot(
        &self,
        bot: &Bot,
        owner_id: &str,
        message: String,
        set_online: bool,
    ) -> AppResult<()> {
        match self.system_bot_dm(bot, owner_id).await? {
            Some(channel) => {
                self.post_as_system_bot(bot, &channel, message, set_online)
                    .await
            }
            None => Ok(()),
        }
    }

    /// The owner's DM with the system bot, or `None` (logged) when this server would forward.
    async fn system_bot_dm(
        &self,
        bot: &Bot,
        owner_id: &str,
    ) -> AppResult<Option<mm_model::channel::Channel>> {
        match self
            .get_or_create_direct_channel(
                &crate::plugin_hooks::HookContext::default(),
                None,
                owner_id,
                &bot.user_id,
            )
            .await?
        {
            ChannelCreate::Created(channel) => Ok(Some(*channel)),
            ChannelCreate::Forward(reason) => {
                tracing::warn!(
                    user_id = owner_id,
                    reason,
                    "a job cannot forward the system bot's DM; the owner is not told"
                );
                Ok(None)
            }
        }
    }

    async fn post_as_system_bot(
        &self,
        bot: &Bot,
        channel: &mm_model::channel::Channel,
        message: String,
        set_online: bool,
    ) -> AppResult<()> {
        let post = Post {
            channel_id: channel.id.clone(),
            message,
            post_type: POST_TYPE_DEFAULT.to_owned(),
            user_id: bot.user_id.clone(),
            ..Post::default()
        };
        match self
            .create_post(
                post,
                channel,
                &Session::default(),
                CreatePostFlags {
                    set_online,
                    ..CreatePostFlags::default()
                },
                &crate::plugin_hooks::HookContext::default(),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(PrepareError::App(err)) => Err(err),
            Err(PrepareError::Unreproducible(reason)) => {
                tracing::warn!(channel_id = %channel.id, reason, "a job cannot forward the system bot's post; the owner is not told");
                Ok(())
            }
        }
    }
}

/// `i18n.GetUserTranslations(locale)(id, params)`.
async fn translate(locale: &str, id: &str, params: Option<&crate::i18n::Params>) -> String {
    match crate::i18n::translations().await {
        Some(bundle) => bundle.translate_with(locale, id, params),
        None => id.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = mm_model::license::DAY_IN_MILLISECONDS;

    /// The bucket is the smallest threshold at least the time left: 2 days left is the 3-day
    /// bucket, exactly 1 day the 1-day one, past 7 days none, expired none.
    #[test]
    fn the_bucket_is_the_smallest_threshold_covering_the_time_left() {
        let now = 1_000 * DAY;
        assert_eq!(access_token_expiry_bucket(now + DAY / 2, now), 1);
        assert_eq!(access_token_expiry_bucket(now + DAY, now), 1);
        assert_eq!(access_token_expiry_bucket(now + DAY + 1, now), 3);
        assert_eq!(access_token_expiry_bucket(now + 2 * DAY, now), 3);
        assert_eq!(access_token_expiry_bucket(now + 3 * DAY, now), 3);
        assert_eq!(access_token_expiry_bucket(now + 5 * DAY, now), 7);
        assert_eq!(access_token_expiry_bucket(now + 7 * DAY, now), 7);
        assert_eq!(access_token_expiry_bucket(now + 7 * DAY + 1, now), 0);
        assert_eq!(
            access_token_expiry_bucket(now, now),
            0,
            "expiring now is expired"
        );
        assert_eq!(access_token_expiry_bucket(now - 1, now), 0);
    }

    fn token(expires_at: i64, last_notified_at: Option<i64>) -> UserAccessToken {
        UserAccessToken {
            expires_at,
            last_notified_at,
            ..UserAccessToken::default()
        }
    }

    /// A warning covers the bucket when it was sent with no more than the bucket left — at the
    /// boundary included — and a warning from a larger bucket does not cover a smaller one.
    #[test]
    fn a_warning_covers_its_bucket_from_the_boundary_on() {
        let expires = 100 * DAY;
        assert!(!already_warned_at(&token(expires, None), 3));
        assert!(already_warned_at(
            &token(expires, Some(expires - 3 * DAY)),
            3
        ));
        assert!(!already_warned_at(
            &token(expires, Some(expires - 3 * DAY - 1)),
            3
        ));
        assert!(
            !already_warned_at(&token(expires, Some(expires - 5 * DAY)), 3),
            "warned in the 7-day bucket, not yet in the 3-day one"
        );
        assert!(already_warned_at(
            &token(expires, Some(expires - DAY / 2)),
            1
        ));
    }
}
