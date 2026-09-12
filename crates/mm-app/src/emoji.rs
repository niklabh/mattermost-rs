//! Port of `server/channels/app/emoji.go`: the single-emoji reads, the list and search, and the
//! two writes `POST /api/v4/emoji` and `DELETE /api/v4/emoji/{emoji_id}` sit on.

use mm_model::emoji::Emoji;
use mm_model::utils::{AppError, AppResult};
use mm_store::emoji_store::EmojiStore;
use mm_store::reaction_store::ReactionStore;

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

// -------------------------------------------------------------------------------------------
// writes: CreateEmoji / DeleteEmoji
// -------------------------------------------------------------------------------------------

/// Port of `app.MaxEmojiFileSize` (app/emoji.go:33) — `1 << 19`, 512 KiB.
pub const MAX_EMOJI_FILE_SIZE: i64 = 1 << 19;

/// Port of `app.MaxEmojiWidth` / `MaxEmojiHeight` (app/emoji.go:34) — above this the image is
/// **resized**, not refused.
pub const MAX_EMOJI_WIDTH: i64 = 128;
/// See [`MAX_EMOJI_WIDTH`].
pub const MAX_EMOJI_HEIGHT: i64 = 128;

/// Port of `app.MaxEmojiOriginalWidth` / `MaxEmojiOriginalHeight` (app/emoji.go:36) — above this
/// the upload is **refused**, with the two limits in the error's i18n params.
pub const MAX_EMOJI_ORIGINAL_WIDTH: i64 = 1028;
/// See [`MAX_EMOJI_ORIGINAL_WIDTH`].
pub const MAX_EMOJI_ORIGINAL_HEIGHT: i64 = 1028;

/// The `image` part of the multipart form, as `CreateEmoji` needs it.
///
/// Go carries a `*multipart.Form` all the way down and reaches into `Form.File["image"]`; this
/// carries the one part that matters, because the multipart shape is the handler's business.
#[derive(Debug, Clone, Copy)]
pub struct EmojiUpload<'a> {
    /// `imageData[0].Filename`. It picks the GIF branch in `uploadEmojiImage` and nothing else —
    /// the stored path is derived from the emoji id, never from this.
    pub filename: &'a str,
    /// The part's bytes, already buffered. Capped at [`MAX_EMOJI_FILE_SIZE`] by the handler.
    pub data: &'a [u8],
}

impl App {
    /// Port of `app.App.CreateEmoji` (app/emoji.go:40).
    ///
    /// # The order of the seven refusals is the wire format
    ///
    /// 1. `EnableCustomEmoji` off → **403** `api.emoji.disabled.app_error`. Unreachable through
    ///    the route: `createEmoji` checks the same setting first and answers **501**.
    /// 2. `FileSettings.DriverName` empty → 403 `api.emoji.storage.app_error`.
    /// 3. `PreSave` then `IsValid` → whatever `model.emoji.*` error the name or the ids earn.
    ///    **The id is blanked first**, so a client cannot choose an emoji's id or overwrite one.
    /// 4. `creator_id` is not the session's user → 403 `api.emoji.create.other_user.app_error`.
    /// 5. A live emoji already holds the name → 400 `api.emoji.create.duplicate.app_error`.
    /// 6. No `image` part → 400 `api.context.invalid_body_param.app_error`, whose `Name` is the
    ///    literal **`createEmoji`** and not `image`, and whose `where` is `Context`.
    /// 7. The image itself — see [`App::upload_emoji_image`].
    ///
    /// Steps 3 and 5 are the pair a reader is most likely to swap: validating *after* the
    /// duplicate check would answer "already taken" for a name that is not a legal name at all.
    ///
    /// # The file is written before the row, and nothing cleans up
    ///
    /// `uploadEmojiImage` runs at step 7 and `Store().Emoji().Save` after it, so a failed insert
    /// leaves the image orphaned under `emoji/<id>/image`. Go's own comment at step 3 says the
    /// validation is deliberately early "so that we don't have to clean up orphaned files"; the
    /// gap between the write and the insert is what remains of that.
    ///
    /// # `PreSave` runs twice
    ///
    /// Once here and once inside [`mm_store::EmojiStore::save`]. The second call keeps the id
    /// (non-empty by then) and **moves `create_at` and `update_at` again**, so the row's
    /// timestamps are the store's, not this function's. Reproduced rather than hoisted.
    #[tracing::instrument(skip_all, fields(emoji_name, emoji_id, bytes))]
    pub async fn create_emoji(
        &self,
        session_user_id: &str,
        mut emoji: Emoji,
        image: Option<EmojiUpload<'_>>,
    ) -> Result<Emoji, PrepareError> {
        self.emoji_storage_available("CreateEmoji")
            .map_err(PrepareError::App)?;

        // "wipe the emoji id so that existing emojis can't get overwritten" (app/emoji.go:51).
        emoji.id = String::new();

        emoji.pre_save();
        emoji.is_valid().map_err(PrepareError::App)?;
        tracing::Span::current().record("emoji_name", &emoji.name);
        tracing::Span::current().record("emoji_id", &emoji.id);

        if emoji.creator_id != session_user_id {
            return Err(PrepareError::App(AppError::boxed(
                "CreateEmoji",
                "api.emoji.create.other_user.app_error",
                None,
                String::new(),
                403,
            )));
        }

        // `err == nil && existingEmoji != nil` — a store *failure* is not a duplicate, and a
        // not-found is a store failure here. So an unreachable database does not refuse the
        // create at this step; it fails at the insert instead.
        if self.store().emoji().get_by_name(&emoji.name).await.is_ok() {
            return Err(PrepareError::App(AppError::boxed(
                "CreateEmoji",
                "api.emoji.create.duplicate.app_error",
                None,
                String::new(),
                400,
            )));
        }

        let Some(image) = image else {
            let mut params: std::collections::HashMap<String, serde_json::Value> =
                std::collections::HashMap::new();
            // `map[string]any{"Name": "createEmoji"}` — the *handler's* name, not the field's.
            params.insert(
                "Name".to_owned(),
                serde_json::Value::String("createEmoji".to_owned()),
            );
            return Err(PrepareError::App(AppError::boxed(
                "Context",
                "api.context.invalid_body_param.app_error",
                Some(params),
                String::new(),
                400,
            )));
        };
        tracing::Span::current().record("bytes", image.data.len());

        self.upload_emoji_image(&emoji.id, image).await?;

        let saved = self
            .store()
            .emoji()
            .save(emoji)
            .await
            // **Every** store failure is one 500 with one id, including a validation error the
            // store raises — Go wraps unconditionally here rather than re-raising the `AppError`.
            .map_err(|err| {
                tracing::error!(error = %err, "emoji insert failed");
                PrepareError::App(AppError::boxed(
                    "CreateEmoji",
                    "app.emoji.create.internal_error",
                    None,
                    String::new(),
                    500,
                ))
            })?;

        self.publish_emoji_added(&saved).await;
        Ok(saved)
    }

    /// Port of the `emoji_added` broadcast at the end of `CreateEmoji` (app/emoji.go:87).
    ///
    /// **The emoji is a JSON string inside `data`, not a nested object** — `message.Add("emoji",
    /// string(emojiJSON))` — the same double-encoding the reaction events use, and the same one a
    /// client decodes twice.
    ///
    /// The broadcast has no team, channel or user, so the hub sends it to **everyone connected**:
    /// a custom emoji is server-wide. Go answers 500 `api.marshal_error` if the marshal fails;
    /// that branch cannot fire for a six-field struct of `String`s and `i64`s, so this logs
    /// instead of inventing an error path the type system rules out.
    async fn publish_emoji_added(&self, emoji: &Emoji) {
        let mut message = mm_model::websocket_message::WebSocketEvent::new(
            mm_model::websocket_message::WEBSOCKET_EVENT_EMOJI_ADDED,
            "",
            "",
            "",
            None,
            "",
        );
        match serde_json::to_string(emoji) {
            Ok(json) => message.add("emoji", serde_json::Value::String(json)),
            Err(err) => {
                tracing::error!(error = %err, "failed to encode the new emoji for the websocket");
                return;
            }
        }
        self.publish(message).await;
    }

    /// Port of `app.App.uploadEmojiImage` (app/emoji.go:105).
    ///
    /// # What this port answers, and what it hands to Go
    ///
    /// Go decodes the image header, refuses anything over 1028×1028, counts the frames of a GIF,
    /// and then either writes the bytes through untouched (≤128×128) or **resizes and re-encodes**
    /// them. The resize is `imaging.Fit` followed by `EncodePNG` or `gif.EncodeAll`, and no second
    /// implementation reproduces those bytes — so the resize path is
    /// [`PrepareError::Unreproducible`] and the request is forwarded. So is every format whose
    /// header this port does not measure exactly (see [`crate::imaging::decode_config`]), and so
    /// is any filename that is not `.png`, which is what keeps the GIF frame walk out of reach.
    ///
    /// Both forwarding points sit **before** the only write, so a forwarded request has left
    /// nothing behind in the file backend. That is not incidental: `WriteFile` is the last
    /// statement of every branch.
    ///
    /// # The two refusals that are answered here
    ///
    /// * No registered decoder claimed the bytes → 400 `api.emoji.upload.image.app_error`.
    /// * Over 1028 in either dimension → 400
    ///   `api.emoji.upload.large_image.too_large.app_error`, carrying `MaxWidth` **and**
    ///   `MaxHeight` as i18n params. This check is `>`, not `>=`: exactly 1028 is accepted.
    ///
    /// Their order matters and is Go's: a 2000-pixel image that is not an image at all is the
    /// decode error, never the size one.
    #[tracing::instrument(skip_all, fields(format, width, height))]
    async fn upload_emoji_image(
        &self,
        id: &str,
        image: EmojiUpload<'_>,
    ) -> Result<(), PrepareError> {
        let (width, height) = match crate::imaging::decode_config(image.data) {
            crate::imaging::ImageConfig::NoFormat => {
                return Err(PrepareError::App(AppError::boxed(
                    "uploadEmojiImage",
                    "api.emoji.upload.image.app_error",
                    None,
                    String::new(),
                    400,
                )));
            }
            crate::imaging::ImageConfig::Undecidable(why) => {
                tracing::debug!(why, "emoji image header is not measured here");
                return Err(PrepareError::Unreproducible(
                    "the emoji image's format is not decoded here",
                ));
            }
            crate::imaging::ImageConfig::Known {
                format,
                width,
                height,
            } => {
                tracing::Span::current().record("format", format);
                tracing::Span::current().record("width", width);
                tracing::Span::current().record("height", height);
                (width, height)
            }
        };

        if width > MAX_EMOJI_ORIGINAL_WIDTH || height > MAX_EMOJI_ORIGINAL_HEIGHT {
            let mut params: std::collections::HashMap<String, serde_json::Value> =
                std::collections::HashMap::new();
            params.insert(
                "MaxWidth".to_owned(),
                serde_json::Value::from(MAX_EMOJI_ORIGINAL_WIDTH),
            );
            params.insert(
                "MaxHeight".to_owned(),
                serde_json::Value::from(MAX_EMOJI_ORIGINAL_HEIGHT),
            );
            return Err(PrepareError::App(AppError::boxed(
                "uploadEmojiImage",
                "api.emoji.upload.large_image.too_large.app_error",
                Some(params),
                String::new(),
                400,
            )));
        }

        // `isGIF` decides whether `CountGIFFrames` runs, and it is read off the **filename**.
        // Anything but a certain `.png` goes to Go rather than being guessed at.
        if !crate::imaging::filename_is_certainly_png(image.filename) {
            return Err(PrepareError::Unreproducible(
                "only a .png filename is certain to skip the GIF frame walk",
            ));
        }

        if width > MAX_EMOJI_WIDTH || height > MAX_EMOJI_HEIGHT {
            return Err(PrepareError::Unreproducible(
                "the emoji image needs resizing, whose output bytes are not reproducible here",
            ));
        }

        self.write_file(image.data, &emoji_image_path(id)).await?;
        Ok(())
    }

    /// Port of `app.App.DeleteEmoji` (app/emoji.go:180).
    ///
    /// # Only the store failure can fail the call
    ///
    /// The image move and the reaction sweep that follow it are `mlog.Warn`-and-carry-on in Go,
    /// so an emoji whose image cannot be renamed is still deleted and the route still answers
    /// `{"status":"OK"}`. Reproduced: swallowing those two is the behaviour, not an oversight.
    ///
    /// Both error ids carry `id=<emoji id>` in `detailed_error`, and the **404 id has no
    /// `.app_error` suffix** (`app.emoji.delete.no_results`) where the 500 does
    /// (`app.emoji.delete.app_error`) — the kind of asymmetry a transcription regularises away.
    #[tracing::instrument(skip_all, fields(emoji_id = %emoji.id, emoji_name = %emoji.name))]
    pub async fn delete_emoji(&self, emoji: &Emoji) -> AppResult {
        self.store()
            .emoji()
            .delete(&emoji.id, mm_model::utils::get_millis())
            .await
            .map_err(|err| {
                let missing = err.is_not_found();
                if !missing {
                    tracing::error!(error = %err, "emoji delete failed");
                }
                AppError::boxed(
                    "DeleteEmoji",
                    if missing {
                        "app.emoji.delete.no_results"
                    } else {
                        "app.emoji.delete.app_error"
                    },
                    None,
                    format!("id={}", emoji.id),
                    if missing { 404 } else { 500 },
                )
            })?;

        self.delete_emoji_image(&emoji.id).await;
        self.delete_reactions_for_emoji(&emoji.name).await;
        Ok(())
    }

    /// Port of `app.App.deleteEmojiImage` (app/emoji.go:381).
    ///
    /// A **rename, not a delete**: `emoji/<id>/image` becomes `emoji/<id>/image_deleted`, so the
    /// bytes survive a delete and `GET /emoji/{id}/image` stops finding them. The destination
    /// path is built inline in Go — `"emoji/"+id+"/image_deleted"` — rather than through
    /// `getEmojiImagePath`, so the two spellings live side by side there and here.
    async fn delete_emoji_image(&self, id: &str) {
        if let Err(err) = self
            .move_file(&emoji_image_path(id), &format!("emoji/{id}/image_deleted"))
            .await
        {
            tracing::warn!(error = %err, emoji_id = id, "Failed to rename image when deleting emoji");
        }
    }

    /// Port of `app.App.deleteReactionsForEmoji` (app/emoji.go:387).
    async fn delete_reactions_for_emoji(&self, emoji_name: &str) {
        if let Err(err) = self
            .store()
            .reaction()
            .delete_all_with_emoji_name(emoji_name)
            .await
        {
            tracing::warn!(
                error = %err,
                emoji_name,
                "Unable to delete reactions when deleting emoji"
            );
        }
    }
}
