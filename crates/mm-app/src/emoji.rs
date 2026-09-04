//! Port of the single-emoji reads in `server/channels/app/emoji.go`.

use mm_model::emoji::Emoji;
use mm_model::utils::{AppError, AppResult};
use mm_store::emoji_store::EmojiStore;

use crate::App;

impl App {
    /// Port of `app.App.GetEmoji` (app/emoji.go:196).
    ///
    /// # Two config gates, and both answer **403**, not 501
    ///
    /// The handler above already refuses a disabled `EnableCustomEmoji` with **501**
    /// (`api.emoji.disabled.app_error`, api4/emoji.go:214), so this second copy of the same
    /// check with the same id and a *different status* is unreachable from the REST route. It
    /// is ported anyway because the id is identical and the status is not: a future caller that
    /// skips the handler check — a plugin path, a websocket handler — would see the 403, and a
    /// port that dropped the check here would answer 200 instead of failing.
    ///
    /// The second gate, `FileSettings.DriverName == ""`, has no handler-level twin, so it is
    /// the only 403 this function can actually produce today. `FileSettings.isValid`
    /// (config.go:4645) restricts the driver to three non-empty names, so reaching it needs a
    /// configuration the Go server itself would reject — see [`crate::config::Config`].
    ///
    /// # The not-found branch returns a value Go then throws away
    ///
    /// Go writes `return emoji, model.NewAppError(...)` in *both* store branches, where `emoji`
    /// is the nil the store handed back. `AppResult` has no room for that and needs none: the
    /// handler checks the error first and never reads the value.
    #[tracing::instrument(skip(self), fields(emoji_id = %emoji_id))]
    pub async fn get_emoji(&self, emoji_id: &str) -> AppResult<Emoji> {
        self.emoji_storage_available("GetEmoji")?;

        self.store().emoji().get(emoji_id).await.map_err(|err| {
            if err.is_not_found() {
                AppError::boxed(
                    "GetEmoji",
                    "app.emoji.get.no_result",
                    None,
                    String::new(),
                    404,
                )
            } else {
                tracing::error!(error = %err, "emoji lookup failed");
                AppError::boxed(
                    "GetEmoji",
                    "app.emoji.get.app_error",
                    None,
                    String::new(),
                    500,
                )
            }
        })
    }

    /// Port of `app.App.GetEmojiByName` (app/emoji.go:219).
    ///
    /// Identical in shape to [`Self::get_emoji`] and different in every string: the `where`
    /// field is `GetEmojiByName` and the two error ids are `app.emoji.get_by_name.no_result`
    /// and `app.emoji.get_by_name.app_error`. Clients branch on the id, so the duplication is
    /// wire format rather than something to factor out.
    #[tracing::instrument(skip(self), fields(emoji_name = %emoji_name))]
    pub async fn get_emoji_by_name(&self, emoji_name: &str) -> AppResult<Emoji> {
        self.emoji_storage_available("GetEmojiByName")?;

        self.store()
            .emoji()
            .get_by_name(emoji_name)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    AppError::boxed(
                        "GetEmojiByName",
                        "app.emoji.get_by_name.no_result",
                        None,
                        String::new(),
                        404,
                    )
                } else {
                    tracing::error!(error = %err, "emoji lookup failed");
                    AppError::boxed(
                        "GetEmojiByName",
                        "app.emoji.get_by_name.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })
    }

    /// Port of `app.App.GetEmojiList` (app/emoji.go:99).
    ///
    /// # It has none of the gates the single reads have
    ///
    /// No `EnableCustomEmoji` check and no `FileSettings.DriverName` check —
    /// [`Self::emoji_storage_available`] is not called here at all. The handler's own 501 is the
    /// *only* thing standing between a disabled server and this query, which is the reverse of
    /// `getEmoji`, where the app layer would refuse even if the handler did not.
    ///
    /// # `page * per_page` is computed here, in `int`
    ///
    /// Go multiplies before the store sees either number and passes the product as `offset`.
    /// `web.ParamsFromRequest` floors `page` at 0 and clamps `per_page` to 200, so the product
    /// cannot be negative and cannot overflow an `i64`.
    ///
    /// One error id, `app.emoji.get_list.internal_error`, and no not-found branch: an offset
    /// past the end of the table is an empty list, not a 404.
    #[tracing::instrument(skip(self), fields(page, per_page, sort_by_name))]
    pub async fn get_emoji_list(
        &self,
        page: i64,
        per_page: i64,
        sort_by_name: bool,
    ) -> AppResult<Vec<Emoji>> {
        self.store()
            .emoji()
            .get_list(page * per_page, per_page, sort_by_name)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "emoji list lookup failed");
                AppError::boxed(
                    "GetEmojiList",
                    "app.emoji.get_list.internal_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// The two config gates both single-emoji reads open with, in Go's order.
    ///
    /// `where_` is threaded through because it is on the wire — `AppError.where` is serialised
    /// — so the shared helper cannot pick a name of its own. Order matters for the same reason:
    /// a server with custom emoji off *and* no file driver answers `api.emoji.disabled`, never
    /// `api.emoji.storage`.
    fn emoji_storage_available(&self, where_: &'static str) -> AppResult {
        if !self.config().enable_custom_emoji {
            return Err(AppError::boxed(
                where_,
                "api.emoji.disabled.app_error",
                None,
                String::new(),
                403,
            ));
        }

        if self.config().file_driver_name.is_empty() {
            return Err(AppError::boxed(
                where_,
                "api.emoji.storage.app_error",
                None,
                String::new(),
                403,
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    /// The gate order is wire format: with both settings off, Go reaches the *first* check and
    /// never the second, so the id a client sees is `disabled`, not `storage`.
    #[tokio::test]
    async fn the_disabled_gate_precedes_the_storage_gate() {
        let config = Config {
            enable_custom_emoji: false,
            file_driver_name: String::new(),
            ..Config::default()
        };
        let app = crate::App::with_config(unreachable_store(), config);
        let err = app
            .emoji_storage_available("GetEmoji")
            .expect_err("both gates are shut");
        assert_eq!(err.id, "api.emoji.disabled.app_error");
        assert_eq!(err.status_code, 403);
        assert_eq!(err.where_, "GetEmoji");
    }

    /// The gate that is actually reachable past the handler's own 501.
    #[tokio::test]
    async fn an_empty_file_driver_is_the_storage_error() {
        let config = Config {
            file_driver_name: String::new(),
            ..Config::default()
        };
        let app = crate::App::with_config(unreachable_store(), config);
        let err = app
            .emoji_storage_available("GetEmojiByName")
            .expect_err("no file driver");
        assert_eq!(err.id, "api.emoji.storage.app_error");
        assert_eq!(err.status_code, 403);
        assert_eq!(err.where_, "GetEmojiByName");
    }

    /// Go's defaults open both gates, which is why the routes work at all on a stock server.
    #[tokio::test]
    async fn gos_defaults_pass_both_gates() {
        let app = crate::App::with_config(unreachable_store(), Config::default());
        assert!(app.emoji_storage_available("GetEmoji").is_ok());
    }

    /// A pool that is never connected: these three tests never reach the store, and a real
    /// connection would put a 30-second `acquire_timeout` in a unit suite (see CLAUDE.md).
    /// `connect_lazy` still wants a reactor to exist, which is why the tests above are
    /// `#[tokio::test]` despite testing a synchronous function.
    fn unreachable_store() -> mm_store::SqlStore {
        mm_store::SqlStore::from_pool(
            sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://unused/unused")
                .expect("a lazy pool needs no server"),
        )
    }
}
