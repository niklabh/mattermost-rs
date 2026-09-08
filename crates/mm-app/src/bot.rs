//! Port of `App.GetBot` and `App.GetBots` (channels/app/bot.go:333, 348).
//!
//! Two calls, and the only content is the error split — which is the same split with a different
//! answer on each side:
//!
//! | | miss | anything else |
//! |---|---|---|
//! | `GetBot` | `MakeBotNotFoundError` — **404**, `store.sql_bot.get.missing.app_error`, and it carries a `user_id` param | `app.bot.getbot.internal_error`, 500 |
//! | `GetBots` | — a query matching nothing is an empty list | `app.bot.getbots.internal_error`, 500 |
//!
//! The 404's id is a **store** id reached through the app layer, and its `where` is
//! `SqlBotStore.Get` rather than `GetBot`. Both are on the wire, so both are reproduced verbatim.

use mm_model::bot::{Bot, BotGetOptions, BotList, make_bot_not_found_error};
use mm_model::utils::{AppError, AppResult};
use mm_store::{BotStore, StoreError};

use crate::App;

impl App {
    /// Port of `App.GetBot` (bot.go:333).
    ///
    /// **The not-found error is the same one the handler raises for a permission failure.** Go's
    /// comment says why: "the errors must be the same in both cases to avoid leaking that a user
    /// is a bot". So a caller who may not read this bot and a caller asking about an id that does
    /// not exist get byte-identical answers — see `mm_api::bots::get_bot`, which depends on it.
    #[tracing::instrument(skip(self), fields(bot_user_id = %bot_user_id, include_deleted))]
    pub async fn get_bot(&self, bot_user_id: &str, include_deleted: bool) -> AppResult<Bot> {
        self.store()
            .bot()
            .get(bot_user_id, include_deleted)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    // Go passes `nfErr.ID` — the id the store put in its `ErrNotFound`, which is
                    // the bot user id it was asked for.
                    return make_bot_not_found_error("SqlBotStore.Get", bot_user_id);
                }
                tracing::error!(error = ?err, "bot lookup failed");
                AppError::boxed(
                    "GetBot",
                    "app.bot.getbot.internal_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `App.GetBots` (bot.go:348). One error, always a 500.
    #[tracing::instrument(skip_all, fields(owner_id = %options.owner_id, found))]
    pub async fn get_bots(&self, options: &BotGetOptions) -> AppResult<BotList> {
        let bots = self
            .store()
            .bot()
            .get_all(options)
            .await
            .map_err(get_bots_error)?;
        tracing::Span::current().record("found", bots.0.len());
        Ok(bots)
    }
}

fn get_bots_error(err: StoreError) -> Box<AppError> {
    tracing::error!(error = ?err, "bot list lookup failed");
    AppError::boxed(
        "GetBots",
        "app.bot.getbots.internal_error",
        None,
        String::new(),
        500,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 404 carries the bot's id as a **parameter**, not only in the message. Clients read
    /// `params`, and an error built without it is a different document.
    #[test]
    fn the_not_found_error_is_the_stores_id_with_a_user_id_param() {
        let err = make_bot_not_found_error("SqlBotStore.Get", "rcw3d9njxiy6pquw79ux5wqxjw");
        assert_eq!(err.id, "store.sql_bot.get.missing.app_error");
        assert_eq!(err.status_code, 404);
        assert_eq!(err.where_, "SqlBotStore.Get");
        assert_eq!(
            err.params
                .as_ref()
                .and_then(|p| p.get("user_id"))
                .and_then(|v| v.as_str()),
            Some("rcw3d9njxiy6pquw79ux5wqxjw")
        );
    }

    /// A list failure is a 500 with a **different id** from the single-bot one — `getbots`, not
    /// `getbot`. One character, and it is what a client branches on.
    #[test]
    fn the_list_error_is_its_own_id() {
        let err = get_bots_error(StoreError::NotFound {
            entity: "Bot",
            criteria: "user_id=x".to_owned(),
        });
        assert_eq!(err.id, "app.bot.getbots.internal_error");
        assert_eq!(err.status_code, 500);
    }
}
