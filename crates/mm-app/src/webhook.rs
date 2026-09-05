//! Port of the three `App` functions behind `getIncomingHooks` (channels/app/webhook.go:645,
//! :658, :675).
//!
//! Each is the same shape: a config gate answering **501**, then one store call wrapped into a
//! 500. The gate is checked *per call*, which matters for `include_total_count` — the handler
//! makes two of them and either can be the one that refuses.

use mm_model::incoming_webhook::IncomingWebhook;
use mm_model::utils::{AppError, AppResult};
use mm_store::{StoreError, WebhookStore};

use crate::App;

/// `api.incoming_webhook.disabled.app_error` — a **501**, not a 403.
///
/// The distinction is on the wire and clients branch on it: a disabled feature is "this server
/// does not do that", where a permission failure is "not you". Go uses `StatusNotImplemented`
/// for every one of the three.
const DISABLED_ERROR: &str = "api.incoming_webhook.disabled.app_error";

impl App {
    /// Port of `App.GetIncomingWebhooksForTeamPageByUser` (webhook.go:645).
    #[tracing::instrument(skip_all, fields(team_id, user_id, page, per_page, found))]
    pub async fn get_incoming_webhooks_for_team_page_by_user(
        &self,
        team_id: &str,
        user_id: &str,
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<IncomingWebhook>> {
        if !self.config().enable_incoming_webhooks {
            return Err(disabled("GetIncomingWebhooksForTeamPage"));
        }

        let hooks = self
            .store()
            .webhook()
            .get_incoming_by_team_by_user(team_id, user_id, page * per_page, per_page)
            .await
            .map_err(|err| store_failure("GetIncomingWebhooksForTeamPage", err))?;

        tracing::Span::current().record("found", hooks.len());
        Ok(hooks)
    }

    /// Port of `App.GetIncomingWebhooksPageByUser` (webhook.go:658).
    ///
    /// Note the `where` string Go reports differs between the two — `GetIncomingWebhooksForTeamPage`
    /// (without the `ByUser` suffix its function name carries) versus
    /// `GetIncomingWebhooksPageByUser`. Both are reproduced verbatim; the field is not on the wire
    /// today, but it is the one part of an `AppError` that says which of two near-identical
    /// functions produced it.
    #[tracing::instrument(skip_all, fields(user_id, page, per_page, found))]
    pub async fn get_incoming_webhooks_page_by_user(
        &self,
        user_id: &str,
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<IncomingWebhook>> {
        if !self.config().enable_incoming_webhooks {
            return Err(disabled("GetIncomingWebhooksPageByUser"));
        }

        let hooks = self
            .store()
            .webhook()
            .get_incoming_list_by_user(user_id, page * per_page, per_page)
            .await
            .map_err(|err| store_failure("GetIncomingWebhooksPageByUser", err))?;

        tracing::Span::current().record("found", hooks.len());
        Ok(hooks)
    }

    /// Port of `App.GetIncomingWebhooksCount` (webhook.go:675).
    ///
    /// Its 500 carries a **different id** from the two list functions —
    /// `app.webhooks.get_incoming_count.app_error` — and a params map naming `TeamID`, `UserID`
    /// and the underlying error. The params are dropped here: Go interpolates them into a
    /// translated message we do not produce ([D-092]), and carrying the raw store error into a
    /// client-visible map would leak query detail that `WipeDetailed` exists to withhold.
    #[tracing::instrument(skip_all, fields(team_id, user_id, count))]
    pub async fn get_incoming_webhooks_count(
        &self,
        team_id: &str,
        user_id: &str,
    ) -> AppResult<i64> {
        if !self.config().enable_incoming_webhooks {
            return Err(disabled("GetIncomingWebhooksCount"));
        }

        let count = self
            .store()
            .webhook()
            .analytics_incoming_count(team_id, user_id)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "counting incoming webhooks failed");
                AppError::boxed(
                    "GetIncomingWebhooksCount",
                    "app.webhooks.get_incoming_count.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        tracing::Span::current().record("count", count);
        Ok(count)
    }
}

fn disabled(where_: &str) -> Box<AppError> {
    AppError::boxed(where_, DISABLED_ERROR, None, String::new(), 501)
}

/// The two list functions share one id, `app.webhooks.get_incoming_by_user.app_error`, and differ
/// only in `where`.
fn store_failure(where_: &str, err: StoreError) -> Box<AppError> {
    tracing::error!(caller = where_, error = ?err, "incoming webhook lookup failed");
    AppError::boxed(
        where_,
        "app.webhooks.get_incoming_by_user.app_error",
        None,
        String::new(),
        500,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A disabled feature is **501**, and it is the same id from all three entry points — but
    /// three different `where`s, which is the only thing separating them in a log.
    #[test]
    fn the_disabled_gate_is_a_501_with_one_id_and_three_wheres() {
        for where_ in [
            "GetIncomingWebhooksForTeamPage",
            "GetIncomingWebhooksPageByUser",
            "GetIncomingWebhooksCount",
        ] {
            let err = disabled(where_);
            assert_eq!(err.status_code, 501, "{where_}: not a 403");
            assert_eq!(err.id, DISABLED_ERROR, "{where_}");
            assert_eq!(err.where_, where_);
        }
    }

    /// The count's failure id differs from the lists' — a port that reused one would answer the
    /// wrong `id` for half the route.
    #[test]
    fn the_count_and_the_list_do_not_share_a_failure_id() {
        let list = store_failure(
            "GetIncomingWebhooksPageByUser",
            StoreError::Db {
                context: "boom".to_owned(),
                source: sqlx::Error::RowNotFound,
            },
        );
        assert_eq!(list.id, "app.webhooks.get_incoming_by_user.app_error");
        assert_eq!(list.status_code, 500);
        assert_ne!(list.id, "app.webhooks.get_incoming_count.app_error");
    }
}
