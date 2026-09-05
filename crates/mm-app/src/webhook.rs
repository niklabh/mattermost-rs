//! Port of the `App` functions behind `getIncomingHooks` and `getOutgoingHooks`
//! (channels/app/webhook.go:645, :658, :675, :820, :833, :850).
//!
//! Each is the same shape: a config gate answering **501**, then one store call wrapped into a
//! 500. The gate is checked *per call*, which matters for `include_total_count` — the handler
//! makes two of them and either can be the one that refuses.
//!
//! # The ids are not symmetric, and one of them is a Go copy-paste
//!
//! The incoming trio share `app.webhooks.get_incoming_by_user.app_error` (and the count has its
//! own). The outgoing trio do **not**: the team function reports
//! `app.webhooks.get_outgoing_by_team.app_error` while *both* the channel function and the
//! **whole-server list** report `app.webhooks.get_outgoing_by_channel.app_error`
//! (webhook.go:827) — the unscoped list naming a channel it never had. Reproduced verbatim.
//! Tidying it would make our error id differ from the server we forward to.

use mm_model::incoming_webhook::IncomingWebhook;
use mm_model::outgoing_webhook::OutgoingWebhook;
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

impl App {
    /// Port of `App.GetOutgoingWebhooksPageByUser` (webhook.go:820).
    #[tracing::instrument(skip_all, fields(user_id, page, per_page, found))]
    pub async fn get_outgoing_webhooks_page_by_user(
        &self,
        user_id: &str,
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<OutgoingWebhook>> {
        if !self.config().enable_outgoing_webhooks {
            return Err(outgoing_disabled("GetOutgoingWebhooksPageByUser"));
        }

        let hooks = self
            .store()
            .webhook()
            .get_outgoing_list_by_user(user_id, page * per_page, per_page)
            .await
            .map_err(|err| {
                outgoing_store_failure(
                    "GetOutgoingWebhooksPageByUser",
                    OUTGOING_BY_CHANNEL_ERROR,
                    err,
                )
            })?;

        tracing::Span::current().record("found", hooks.len());
        Ok(hooks)
    }

    /// Port of `App.GetOutgoingWebhooksForChannelPageByUser` (webhook.go:833).
    ///
    /// Its `where` is `GetOutgoingWebhooksForChannelPage` — without the `ByUser` its own name
    /// carries, exactly as the incoming team function drops it.
    #[tracing::instrument(skip_all, fields(channel_id, user_id, page, per_page, found))]
    pub async fn get_outgoing_webhooks_for_channel_page_by_user(
        &self,
        channel_id: &str,
        user_id: &str,
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<OutgoingWebhook>> {
        if !self.config().enable_outgoing_webhooks {
            return Err(outgoing_disabled("GetOutgoingWebhooksForChannelPage"));
        }

        let hooks = self
            .store()
            .webhook()
            .get_outgoing_by_channel_by_user(channel_id, user_id, page * per_page, per_page)
            .await
            .map_err(|err| {
                outgoing_store_failure(
                    "GetOutgoingWebhooksForChannelPage",
                    OUTGOING_BY_CHANNEL_ERROR,
                    err,
                )
            })?;

        tracing::Span::current().record("found", hooks.len());
        Ok(hooks)
    }

    /// Port of `App.GetOutgoingWebhooksForTeamPageByUser` (webhook.go:850) — the one outgoing
    /// function with an id of its own.
    #[tracing::instrument(skip_all, fields(team_id, user_id, page, per_page, found))]
    pub async fn get_outgoing_webhooks_for_team_page_by_user(
        &self,
        team_id: &str,
        user_id: &str,
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<OutgoingWebhook>> {
        if !self.config().enable_outgoing_webhooks {
            return Err(outgoing_disabled("GetOutgoingWebhooksForTeamPageByUser"));
        }

        let hooks = self
            .store()
            .webhook()
            .get_outgoing_by_team_by_user(team_id, user_id, page * per_page, per_page)
            .await
            .map_err(|err| {
                outgoing_store_failure(
                    "GetOutgoingWebhooksForTeamPageByUser",
                    OUTGOING_BY_TEAM_ERROR,
                    err,
                )
            })?;

        tracing::Span::current().record("found", hooks.len());
        Ok(hooks)
    }
}

/// `api.outgoing_webhook.disabled.app_error` — a **different id** from the incoming one, and the
/// same 501.
const OUTGOING_DISABLED_ERROR: &str = "api.outgoing_webhook.disabled.app_error";
/// Reported by the channel function **and** by the unscoped list (webhook.go:827) — see the
/// module note.
const OUTGOING_BY_CHANNEL_ERROR: &str = "app.webhooks.get_outgoing_by_channel.app_error";
const OUTGOING_BY_TEAM_ERROR: &str = "app.webhooks.get_outgoing_by_team.app_error";

fn outgoing_disabled(where_: &str) -> Box<AppError> {
    AppError::boxed(where_, OUTGOING_DISABLED_ERROR, None, String::new(), 501)
}

fn outgoing_store_failure(where_: &str, id: &'static str, err: StoreError) -> Box<AppError> {
    tracing::error!(caller = where_, error = ?err, "outgoing webhook lookup failed");
    AppError::boxed(where_, id, None, String::new(), 500)
}

#[cfg(test)]
mod outgoing_tests {
    use super::*;

    /// The incoming and outgoing disabled errors are **different ids** at the same status. A port
    /// that shared one would tell an integrations page the wrong feature was off.
    #[test]
    fn the_two_disabled_ids_are_distinct() {
        assert_ne!(DISABLED_ERROR, OUTGOING_DISABLED_ERROR);
        assert_eq!(outgoing_disabled("x").status_code, 501);
        assert_eq!(outgoing_disabled("x").id, OUTGOING_DISABLED_ERROR);
    }

    /// **The unscoped outgoing list reports a *channel* error id.** It is Go's copy-paste
    /// (webhook.go:827) and it is deliberate here; this test exists so that "fixing" it fails.
    #[test]
    fn the_unscoped_outgoing_list_reports_the_channel_id() {
        assert_eq!(
            OUTGOING_BY_CHANNEL_ERROR,
            "app.webhooks.get_outgoing_by_channel.app_error"
        );
        assert_ne!(OUTGOING_BY_CHANNEL_ERROR, OUTGOING_BY_TEAM_ERROR);
    }
}
