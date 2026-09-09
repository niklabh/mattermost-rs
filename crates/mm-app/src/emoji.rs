//! Port of the single-emoji reads in `server/channels/app/emoji.go`.

use mm_model::emoji::Emoji;
use mm_model::utils::{AppError, AppResult};
use mm_store::emoji_store::EmojiStore;

use crate::App;
use crate::post::PrepareError;

impl App {
    /// Port of `app.App.GetMultipleEmojiByName` (app/emoji.go:242).
    ///
    /// # It filters the request, not the answer
    ///
    /// Every name that names a **system** emoji is removed before the query runs, in place, with
    /// Go's compacting loop. So `["+1", "mmrsparityx"]` asks the database for one name, and
    /// `["+1"]` asks for none — which is the branch below, and it returns an **empty vec rather
    /// than an error**. A client asking only for built-in emoji gets `[]`, not a 404 and not a
    /// list of the built-ins: this route answers about *custom* emoji only.
    ///
    /// # The config gate here is a 403 and it is unreachable
    ///
    /// `getEmojisByNames` checks `EnableCustomEmoji` first and answers 501, so this second check
    /// — same id, different status — cannot fire through the route. Reproduced because it is
    /// Go's, and because the only thing distinguishing the two is the status a client would see
    /// if the order ever changed.
    #[tracing::instrument(skip_all, fields(asked = names.len(), custom))]
    pub async fn get_multiple_emoji_by_name(&self, names: &[String]) -> AppResult<Vec<Emoji>> {
        if !self.config().enable_custom_emoji {
            return Err(AppError::boxed(
                "GetMultipleEmojiByName",
                "api.emoji.disabled.app_error",
                None,
                String::new(),
                403,
            ));
        }

        let custom: Vec<String> = names
            .iter()
            .filter(|name| mm_model::emoji::get_system_emoji_id(name).is_none())
            .cloned()
            .collect();
        tracing::Span::current().record("custom", custom.len());

        if custom.is_empty() {
            return Ok(Vec::new());
        }

        self.store()
            .emoji()
            .get_multiple_by_name(&custom)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "emoji-by-names lookup failed");
                AppError::boxed(
                    "GetMultipleEmojiByName",
                    "app.emoji.get_by_name.app_error",
                    None,
                    format!("names={custom:?}"),
                    500,
                )
            })
    }

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

    /// Port of `app.App.SearchEmoji` (app/emoji.go:294).
    ///
    /// **One gate, not two.** Unlike [`Self::get_emoji`] and its by-name twin, this checks only
    /// `EnableCustomEmoji` and never `FileSettings.DriverName` — so it cannot answer
    /// `api.emoji.storage.app_error` at all. And unlike them, its handler
    /// (`autocompleteEmojis`, api4/emoji.go:331) carries **no gate of its own**, so this 403 is
    /// the one a client actually sees when custom emoji are off, rather than being shadowed by a
    /// handler-level 501.
    ///
    /// # This 403 is the one that reaches the wire
    ///
    /// Every other emoji route checks `EnableCustomEmoji` in its *handler* and answers 501, which
    /// shadows this. `searchEmojis` has **no handler check**, so `POST /emoji/search` is the one
    /// route where a client sees the 403 — the same feature flag, a different status, depending
    /// on which emoji route was asked. `autocompleteEmojis`, this function's other caller, does
    /// have the handler check.
    ///
    /// The 500's `detailed_error` carries `name=<term>` — the only place in the emoji app layer
    /// that puts a caller's input into an error. `detailed_error` is on the wire but Go leaves it
    /// empty unless the server is in developer mode, so this is reproduced for the log rather
    /// than for the response.
    #[tracing::instrument(skip(self), fields(prefix_only, limit))]
    pub async fn search_emoji(
        &self,
        name: &str,
        prefix_only: bool,
        limit: i64,
    ) -> AppResult<Vec<Emoji>> {
        if !self.config().enable_custom_emoji {
            return Err(AppError::boxed(
                "SearchEmoji",
                "api.emoji.disabled.app_error",
                None,
                String::new(),
                403,
            ));
        }

        self.store()
            .emoji()
            .search(name, prefix_only, limit)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "emoji search failed");
                AppError::boxed(
                    "SearchEmoji",
                    "app.emoji.get_by_name.app_error",
                    None,
                    format!("name={name}"),
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

/// Port of `app.getEmojiImagePath` (app/emoji.go:348).
fn emoji_image_path(id: &str) -> String {
    format!("emoji/{id}/image")
}

impl App {
    /// Port of `app.App.GetEmojiImage` (app/emoji.go:278).
    ///
    /// Returns the bytes and the **format name** `image.DecodeConfig` reports, which the handler
    /// turns straight into `Content-Type: image/<name>` — so a GIF emoji is served as `image/gif`
    /// even though every emoji is stored under a path ending in `image` with no extension.
    ///
    /// # The store read is not the emoji you already have
    ///
    /// It exists to 404 a deleted or absent emoji before touching the backend, and its two error
    /// ids — `app.emoji.get.no_result` and `app.emoji.get.app_error` — are `GetEmoji`'s, not this
    /// function's. Note also that `GetEmojiImage` does **not** call `emoji_storage_available`:
    /// unlike [`App::get_emoji`] it never checks `FileSettings.DriverName`, so a driverless
    /// configuration reaches the backend read and fails there instead of refusing with a 403.
    ///
    /// # The two failures below the store read are both `getEmojiImage`, lowercase
    ///
    /// Go spells the `where` with a lowercase `g` for these two and an uppercase one for the
    /// store read above them — three errors from one function under two different names.
    pub async fn get_emoji_image(
        &self,
        emoji_id: &str,
    ) -> Result<(Vec<u8>, &'static str), PrepareError> {
        self.store()
            .emoji()
            .get(emoji_id)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    AppError::boxed(
                        "GetEmojiImage",
                        "app.emoji.get.no_result",
                        None,
                        String::new(),
                        404,
                    )
                } else {
                    tracing::error!(error = %err, "emoji lookup failed");
                    AppError::boxed(
                        "GetEmojiImage",
                        "app.emoji.get.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })
            .map_err(PrepareError::App)?;

        let image = self
            .read_file(&emoji_image_path(emoji_id))
            .await
            .map_err(|err| match err {
                // `ReadFile`'s own 500 is discarded and re-raised as a **404** — the only place
                // in the file family where a backend failure becomes "not found".
                PrepareError::App(_) => PrepareError::App(AppError::boxed(
                    "getEmojiImage",
                    "api.emoji.get_image.read.app_error",
                    None,
                    String::new(),
                    404,
                )),
                unreproducible => unreproducible,
            })?;

        let Some(format) = crate::imaging::detect_format(&image) else {
            return Err(PrepareError::App(AppError::boxed(
                "getEmojiImage",
                "api.emoji.get_image.decode.app_error",
                None,
                String::new(),
                500,
            )));
        };

        Ok((image, format))
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

    /// `SearchEmoji`'s gate is **its own**, not the shared `emoji_storage_available` — it checks
    /// only `EnableCustomEmoji` and never the file driver, and its handler carries no 501 of its
    /// own. So this 403 is the status a client of `GET /emoji/autocomplete` would actually see,
    /// and nothing over HTTP can reach it on a server with the setting on. Asserted here for the
    /// same reason the gate order above is: the parity suite cannot turn the setting off.
    #[tokio::test]
    async fn search_emoji_refuses_with_a_403_and_never_asks_about_the_file_driver() {
        let config = Config {
            enable_custom_emoji: false,
            ..Config::default()
        };
        let app = crate::App::with_config(unreachable_store(), config);
        let err = app
            .search_emoji("anything", true, 100)
            .await
            .expect_err("custom emoji are off");
        assert_eq!(err.id, "api.emoji.disabled.app_error");
        assert_eq!(
            err.status_code, 403,
            "the app layer's 403, not a handler 501"
        );
        assert_eq!(err.where_, "SearchEmoji");

        // An empty file driver is *not* a refusal here, unlike every other emoji read. With the
        // setting back on, the gate opens and the call goes on to the store — which is
        // unreachable in this test, so it fails as a 500 rather than as a 403.
        let config = Config {
            file_driver_name: String::new(),
            ..Config::default()
        };
        let app = crate::App::with_config(unreachable_store(), config);
        let err = app
            .search_emoji("anything", true, 100)
            .await
            .expect_err("the store is not connected");
        assert_eq!(
            err.id, "app.emoji.get_by_name.app_error",
            "it got past the gate: no file-driver check on this path"
        );
        assert_eq!(err.status_code, 500);
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
