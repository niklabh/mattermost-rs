//! The app layer behind the rest of `api4/post.go`, `api4/report.go`'s two writes and
//! `api4/integration_action.go`: `SetPostReminder`, `RestorePostVersion`, `RevealPost`,
//! `BurnPost`, the `RewriteMessage` gates, `GetPostsForReporting`, `StartUsersBatchExport`,
//! `OpenInteractiveDialog` and the pre-outbound half of the three dialog submits.
//!
//! # Where the integration call is decided, and why it forwards
//!
//! `DoActionRequest` (app/integration_action.go:135) `POST`s to the integration through the
//! outbound guard (`MakeClient(false)`), or straight into the plugin host for a `/plugins/`
//! path, or with the session's bearer token when the URL is the site's own `/plugins/`
//! subtree. The guard's *refusal* is reproduced — [`OutboundDisposition::Refused`] is the 400
//! `api.post.do_action.action_integration.app_error` Go answers when the dial is forbidden —
//! and everything the guard would let through is handed to Go, because a request that reaches
//! an integration and the response handling after it cannot be compared against an oracle
//! whose allow-list is empty. Every decision below is made **before** anything is written.

use std::time::Duration;

use mm_model::integration_action::{
    MAX_DIALOG_FILE_IDS, MAX_DIALOG_SUBMISSION_ID_SHAPED_TOKEN_SCAN, OpenDialogRequest,
    SubmitDialogRequest, validate_action_query,
};
use mm_model::job::{JOB_STATUS_IN_PROGRESS, JOB_STATUS_PENDING, JOB_TYPE_EXPORT_USERS_TO_CSV};
use mm_model::license::{minimum_enterprise_license, minimum_professional_license};
use mm_model::post::{
    POST_PROPS_EXPIRE_AT, POST_TYPE_BURN_ON_READ, POST_TYPE_EPHEMERAL, POST_TYPE_REMINDER, Post,
    PostPatch,
};
use mm_model::post_rest::{ReportPostListResponse, ReportPostQueryParams};
use mm_model::read_receipt::ReadReceipt;
use mm_model::report::{
    REPORT_DURATION_LAST_6_MONTHS, REPORT_DURATION_LAST_30_DAYS, REPORT_DURATION_PREVIOUS_MONTH,
    UserReportOptions,
};
use mm_model::session::Session;
use mm_model::utils::{
    AppError, AppResult, NO_TRANSLATION, StringInterface, StringMap, get_millis, go_json_marshal,
    is_valid_id, new_id, remove_duplicate_strings_non_sort,
};
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_BURN_ON_READ_ALL_REVEALED, WEBSOCKET_EVENT_EPHEMERAL_MESSAGE,
    WEBSOCKET_EVENT_OPEN_DIALOG, WEBSOCKET_EVENT_POST_BURNED, WEBSOCKET_EVENT_POST_REVEALED,
    WebSocketEvent,
};
use mm_store::{
    FileInfoStore, JobStore, PostStore, ReadReceiptStore, StoreError, TeamStore,
    TemporaryPostStore, UserStore,
};

use crate::App;
use crate::http_guard::{GuardError, GuardedClient};
use crate::post::{PrepareError, PreparePostForClientOpts, message_may_contain_a_link};
use crate::post_create::CreatePostFlags;

fn app_error(where_: &'static str, id: &str, status: i32) -> Box<AppError> {
    AppError::boxed(where_, id, None, String::new(), status)
}

/// `time.RFC822` in UTC: `02 Jan 06 15:04 UTC`. `time.Unix(targetTime, 0)` takes any `int64`;
/// chrono refuses one outside its range, and Go's formatting of such a year is not reproduced.
fn rfc822_utc(seconds: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, 0)
        .map(|t| t.format("%d %b %y %H:%M UTC").to_string())
        .unwrap_or_default()
}

/// What `doPostAction` answers before any integration is called.
#[derive(Debug, PartialEq, Eq)]
pub enum PostActionOutcome {
    /// An `openURL` block action: no request is sent and the response carries the location.
    Goto(String),
    /// The action names an integration URL; see [`OutboundDisposition`].
    Outbound(OutboundDisposition),
}

/// What `DoActionRequest` would do with an integration URL, decided without dialling.
#[derive(Debug, PartialEq, Eq)]
pub enum OutboundDisposition {
    /// The guard forbids every address the host resolves to, or the URL does not parse, or the
    /// name does not resolve: Go's `httpClient.Do` fails and the route answers the 400
    /// `api.post.do_action.action_integration.app_error`.
    Refused,
    /// The request would be sent — to the integration, the plugin host, or the site's own
    /// `/plugins/` subtree — and its answer processed. Not reproduced: the caller forwards.
    Forward(&'static str),
}

impl App {
    /// Port of `app.App.revealSingleBurnOnReadPost` (app/post_helpers.go:335) and the one-post
    /// case of `revealBurnOnReadPostsForUser` (:369) — what `GetSinglePost` does to a
    /// burn-on-read row **before** any handler sees it, for the session's user.
    ///
    /// A post of any other type, or the feature off (`isBurnOnReadEnabled`: the flag **and**
    /// the setting), is returned as it came. Otherwise: the **author** gets the content with
    /// `metadata.recipients` — every receipt's user, in store order; a reader **without** a
    /// receipt gets the unrevealed copy — empty message, empty (non-nil) metadata; a reader
    /// whose receipt has **expired** gets nothing — the post is removed from the list and
    /// `revealSingleBurnOnReadPost` answers the 404 `app.post.get.app_error`, which is why
    /// `RevealPost`'s own 403 `read_receipt_expired` is unreachable through the route; and a
    /// reader with a live receipt gets the content with `metadata.expire_at`.
    ///
    /// Two 500s on the way: the temporary row (`app.post.get_post.app_error`) and the receipt
    /// reads (`app.post.get_posts.app_error`).
    #[tracing::instrument(skip(self, post), fields(post_id = %post.id, user_id = %user_id))]
    pub async fn reveal_single_burn_on_read_post(
        &self,
        post: Post,
        user_id: &str,
    ) -> AppResult<Post> {
        if post.post_type != POST_TYPE_BURN_ON_READ || !self.config().burn_on_read() {
            return Ok(post);
        }
        // `post.DeleteAt > 0` is skipped by the loop, leaving the row as it came.
        if post.delete_at > 0 {
            return Ok(post);
        }

        if post.user_id == user_id {
            let mut revealed = self.burn_on_read_content(&post).await?;
            let recipients = self
                .store()
                .read_receipt()
                .get_by_post(&post.id)
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, "read receipts lookup failed");
                    app_error(
                        "RevealBurnOnReadPostsForUser",
                        "app.post.get_posts.app_error",
                        500,
                    )
                })?;
            let metadata = revealed.metadata.get_or_insert_with(Default::default);
            metadata
                .recipients
                .extend(recipients.into_iter().map(|receipt| receipt.user_id));
            return Ok(revealed);
        }

        let receipt = match self.store().read_receipt().get(&post.id, user_id).await {
            Ok(receipt) => Some(receipt),
            Err(err) if err.is_not_found() => None,
            Err(err) => {
                tracing::error!(error = %err, "read receipt lookup failed");
                return Err(app_error(
                    "RevealBurnOnReadPostsForUser",
                    "app.post.get_posts.app_error",
                    500,
                ));
            }
        };
        let Some(receipt) = receipt else {
            // `setUnrevealedPost`: a clone with every metadata list emptied and no message.
            // All five are `omitempty`, so an empty `PostMetadata` is the same bytes.
            let mut unrevealed = post;
            unrevealed.metadata = Some(Default::default());
            unrevealed.message = String::new();
            return Ok(unrevealed);
        };
        if receipt.expire_at < get_millis() {
            return Err(app_error(
                "revealSingleBurnOnReadPost",
                "app.post.get.app_error",
                404,
            ));
        }
        let mut revealed = self.burn_on_read_content(&post).await?;
        revealed
            .metadata
            .get_or_insert_with(Default::default)
            .expire_at = receipt.expire_at;
        Ok(revealed)
    }

    /// Port of `app.App.getBurnOnReadPost` (app/post.go:4032): a clone carrying the temporary
    /// row's message and file ids.
    async fn burn_on_read_content(&self, post: &Post) -> AppResult<Post> {
        let temporary = self
            .store()
            .temporary_post()
            .get(&post.id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "temporary post lookup failed");
                app_error("getBurnOnReadPost", "app.post.get_post.app_error", 500)
            })?;
        let mut clone = post.clone();
        clone.message = temporary.message;
        clone.file_ids = temporary.file_ids;
        Ok(clone)
    }

    /// Port of `app.App.SetPostReminder` (app/post.go:2838).
    ///
    /// The row is written, then the confirmation goes down the user's websocket as an
    /// `ephemeral_message` carrying a `reminder`-typed ephemeral post whose text embeds a
    /// permalink to the reminded post. `PreparePostForClientWithEmbedsAndImages` turns that
    /// permalink into a `permalink` embed — reproduced for a post in a **team** channel, whose
    /// link is `{SiteURL}/{team}/pl/{id}`. A DM or group-channel post gets `{SiteURL}/pl/{id}`,
    /// which `looksLikeAPermalink` rejects, so Go fetches it as an ordinary URL through the
    /// outbound guard and records a `LinkMetadata` row — neither reproduced, and that branch
    /// is refused **before** the reminder row is written so the caller can forward it whole.
    ///
    /// Both store failures are the 500 `<untranslated>` (`model.NoTranslation`), a 404 from
    /// the existence check included: `SetPostReminder`'s `ErrNotFound` is wrapped like any
    /// other error.
    #[tracing::instrument(skip(self, session), fields(post_id = %post_id, user_id = %user_id, target_time))]
    pub async fn set_post_reminder(
        &self,
        session: &Session,
        post_id: &str,
        user_id: &str,
        target_time: i64,
    ) -> Result<(), PrepareError> {
        let reminded = self.get_single_post(post_id, false).await?;
        let ephemeral_root_id = if reminded.root_id.is_empty() {
            reminded.id.clone()
        } else {
            reminded.root_id.clone()
        };

        let metadata = self
            .store()
            .post()
            .get_post_reminder_metadata(post_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "post reminder metadata lookup failed");
                app_error("SetPostReminder", NO_TRANSLATION, 500)
            })?;

        // Decided before the write: see the doc comment.
        if metadata.team_name.is_empty() {
            return Err(PrepareError::Unreproducible(
                "a DM or group-channel permalink is fetched, not previewed",
            ));
        }

        self.store()
            .post()
            .set_post_reminder(post_id, user_id, target_time)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "post reminder write failed");
                app_error("SetPostReminder", NO_TRANSLATION, 500)
            })?;

        let parsed_time = rfc822_utc(target_time);
        let site_url = self.config().site_url.clone().unwrap_or_default();
        let permalink = format!("{site_url}/{}/pl/{post_id}", metadata.team_name);

        let mut props = StringInterface::new();
        props.insert("target_time".to_owned(), serde_json::json!(target_time));
        props.insert(
            "team_name".to_owned(),
            serde_json::Value::String(metadata.team_name.clone()),
        );
        props.insert(
            "post_id".to_owned(),
            serde_json::Value::String(post_id.to_owned()),
        );
        props.insert(
            "username".to_owned(),
            serde_json::Value::String(metadata.username.clone()),
        );
        props.insert(
            "type".to_owned(),
            serde_json::Value::String(POST_TYPE_REMINDER.to_owned()),
        );
        let ephemeral = Post {
            post_type: POST_TYPE_EPHEMERAL.to_owned(),
            id: new_id(),
            create_at: get_millis(),
            user_id: user_id.to_owned(),
            root_id: ephemeral_root_id,
            channel_id: metadata.channel_id.clone(),
            // Go's own comment: "It's okay to keep this non-translated. This is just a
            // fallback." — the webapp renders the props.
            message: format!(
                "You will be reminded about {permalink} by @{} at {parsed_time}",
                metadata.username
            ),
            props: Some(props),
            ..Post::default()
        };

        let mut message = WebSocketEvent::new(
            WEBSOCKET_EVENT_EPHEMERAL_MESSAGE,
            "",
            &ephemeral.channel_id,
            user_id,
            None,
            "",
        );
        // `AddPostActionCookies` walks `props.attachments`; a reminder has none.
        let prepared = self
            .prepare_post_for_client_with_embeds_and_images(
                &ephemeral,
                PreparePostForClientOpts {
                    is_new_post: true,
                    include_priority: true,
                    ..PreparePostForClientOpts::default()
                },
            )
            .await?;
        let _ = session;
        match prepared.to_json() {
            Ok(json) => message.add("post", serde_json::Value::String(json)),
            Err(err) => {
                tracing::warn!(error = %err, "Failed to encode post to JSON");
                message.add("post", serde_json::Value::String(String::new()));
            }
        }
        self.publish(message).await;
        Ok(())
    }

    /// Port of `app.App.RestorePostVersion` (app/post_restore.go:16): four safeguards on the
    /// history row, then `PatchPost` with the old message and file ids under `IsRestorePost`.
    ///
    /// The one thing `IsRestorePost` changes is `processPostFileChanges`: a file the old version
    /// had and the current one does not is **undeleted** (`RestoreForPostByIds`) rather than
    /// re-attached. That branch is not ported; [`App::update_post`] refuses any change to the
    /// file id set before writing, so a restore that would touch files forwards whole.
    #[tracing::instrument(skip(self, session), fields(post_id = %post_id, restore_version_id = %restore_version_id))]
    pub async fn restore_post_version(
        &self,
        session: &Session,
        post_id: &str,
        restore_version_id: &str,
    ) -> Result<(Post, bool), PrepareError> {
        let to_restore = self
            .store()
            .post()
            .get_single(restore_version_id, true)
            .await
            .map_err(|err| {
                let status = if err.is_not_found() { 404 } else { 500 };
                AppError::boxed(
                    "RestorePostVersion",
                    "app.post.restore_post_version.get_single.app_error",
                    None,
                    err.to_string(),
                    status,
                )
            })?;

        if to_restore.original_id != post_id {
            return Err(PrepareError::App(app_error(
                "RestorePostVersion",
                "app.post.restore_post_version.not_an_history_item.app_error",
                400,
            )));
        }
        if to_restore.user_id != session.user_id {
            return Err(PrepareError::App(app_error(
                "RestorePostVersion",
                "app.post.restore_post_version.not_allowed.app_error",
                403,
            )));
        }
        if to_restore.delete_at == 0 {
            return Err(PrepareError::App(app_error(
                "RestorePostVersion",
                "app.post.restore_post_version.not_valid_post_history_item.app_error",
                400,
            )));
        }

        let patch = PostPatch {
            message: Some(to_restore.message),
            file_ids: Some(to_restore.file_ids.unwrap_or_default()),
            ..PostPatch::default()
        };
        self.patch_post(post_id, &patch, session).await
    }

    /// Port of `app.App.RevealPost` (app/post.go:3730) for a reader who is not the author.
    ///
    /// Order is Go's: type check, `expire_at` prop, post expiry, get-or-create the receipt,
    /// receipt expiry, the content from `TemporaryPosts`, `Metadata.ExpireAt`, prepare,
    /// sanitize, and — on a **first** reveal only — the two `post_revealed` events, one to the
    /// author (with `recipients: [reader]`) and one to the reader. The temporary row is read
    /// once up front so a message this port cannot prepare is refused before the receipt is
    /// written; Go's own read of it happens after, and its 500 is kept in that place.
    ///
    /// `SanitizePostMetadataForUser`'s failure is logged and the unsanitized post used, as Go
    /// does; a `PrepareError::Unreproducible` from it cannot happen on a post with no embeds.
    #[tracing::instrument(skip(self, post), fields(post_id = %post.id, user_id = %user_id, first_reveal))]
    pub async fn reveal_post(
        &self,
        post: &Post,
        user_id: &str,
        connection_id: &str,
    ) -> Result<Post, PrepareError> {
        if post.post_type != POST_TYPE_BURN_ON_READ {
            return Err(PrepareError::App(app_error(
                "RevealPost",
                "app.reveal_post.not_burn_on_read.app_error",
                400,
            )));
        }
        // `validateBurnOnReadPost`'s second arm, `post.UserId == session.UserId`, is
        // unreachable: the handler refused it a step earlier with its own id.

        let post_expire_at = extract_post_expiration(post)?;
        let current_time = get_millis();
        if current_time >= post_expire_at {
            return Err(PrepareError::App(app_error(
                "RevealPost",
                "app.reveal_post.post_expired.app_error",
                400,
            )));
        }

        // The forward decision, ahead of the write. A missing row is not decided here: Go
        // reaches its 500 after the receipt exists, and so does this.
        let temporary = self.store().temporary_post().get(&post.id).await;
        if let Ok(temporary) = &temporary {
            if message_may_contain_a_link(&temporary.message)
                && crate::post::sole_permalink_in(
                    &temporary.message,
                    self.config().site_url.as_deref().unwrap_or(""),
                )
                .is_none()
            {
                return Err(PrepareError::Unreproducible(
                    "the revealed message may contain a link",
                ));
            }
        }

        let (receipt, first_reveal) = self
            .get_or_create_read_receipt(post, user_id, post_expire_at, current_time)
            .await?;
        tracing::Span::current().record("first_reveal", first_reveal);

        if receipt.expire_at < current_time {
            return Err(PrepareError::App(app_error(
                "RevealPost",
                "app.reveal_post.read_receipt_expired.error",
                403,
            )));
        }

        // Go reads the temporary row here, after the receipt; its absence is this 500.
        let mut revealed = self.burn_on_read_content(post).await?;
        let _ = temporary;
        revealed
            .metadata
            .get_or_insert_with(Default::default)
            .expire_at = receipt.expire_at;

        let revealed = self
            .prepare_post_for_client_with_embeds_and_images(
                &revealed,
                PreparePostForClientOpts {
                    include_priority: true,
                    retain_content: true,
                    ..PreparePostForClientOpts::default()
                },
            )
            .await?;
        let revealed = match self
            .sanitize_post_metadata_for_user(revealed.clone(), user_id)
            .await
        {
            Ok((sanitized, _)) => sanitized,
            Err(err) => {
                tracing::warn!(error = %err, post_id = %revealed.id, user_id, "Failed to sanitize post metadata for revealed BOR post; proceeding without sanitization");
                revealed
            }
        };

        if first_reveal {
            let json = revealed.to_json().map_err(|err| {
                PrepareError::App(AppError::boxed(
                    "RevealPost",
                    "app.post.marshal.app_error",
                    None,
                    err.to_string(),
                    500,
                ))
            })?;
            let mut to_author = WebSocketEvent::new(
                WEBSOCKET_EVENT_POST_REVEALED,
                "",
                "",
                &revealed.user_id,
                None,
                connection_id,
            );
            to_author.add("post", serde_json::Value::String(json.clone()));
            to_author.add("recipients", serde_json::json!([user_id]));
            self.publish(to_author).await;

            let mut to_reader = WebSocketEvent::new(
                WEBSOCKET_EVENT_POST_REVEALED,
                "",
                "",
                user_id,
                None,
                connection_id,
            );
            to_reader.add("post", serde_json::Value::String(json));
            self.publish(to_reader).await;
        }

        Ok(revealed)
    }

    /// Port of `app.App.getOrCreateReadReceipt` (app/post.go:3857). A first reveal's receipt
    /// expires at `min(post expire_at, now + BurnOnReadDurationSeconds * 1000)`; after it is
    /// written, `updateTemporaryPostIfAllRead` may move the temporary row's expiry to it and
    /// tell the author — a failure there is logged, not returned.
    async fn get_or_create_read_receipt(
        &self,
        post: &Post,
        user_id: &str,
        post_expire_at: i64,
        current_time: i64,
    ) -> Result<(ReadReceipt, bool), PrepareError> {
        match self.store().read_receipt().get(&post.id, user_id).await {
            Ok(receipt) => return Ok((receipt, false)),
            Err(err) if err.is_not_found() => {}
            Err(err) => {
                tracing::error!(error = %err, "read receipt lookup failed");
                return Err(PrepareError::App(app_error(
                    "RevealPost",
                    "app.reveal_post.read_receipt.get.error",
                    500,
                )));
            }
        }

        let read_duration_millis = self.config().burn_on_read_duration_seconds * 1000;
        let receipt = ReadReceipt {
            user_id: user_id.to_owned(),
            post_id: post.id.clone(),
            expire_at: post_expire_at.min(current_time + read_duration_millis),
        };
        self.store()
            .read_receipt()
            .save(&receipt)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "read receipt write failed");
                app_error("RevealPost", "app.reveal_post.read_receipt.save.error", 500)
            })?;

        if let Err(err) = self.update_temporary_post_if_all_read(post, &receipt).await {
            tracing::warn!(error = %err, post_id = %post.id, "Failed to update temporary post expiration after all recipients read");
        }

        Ok((receipt, true))
    }

    /// Port of `app.App.updateTemporaryPostIfAllRead` (app/post.go:3893): when no channel
    /// member but the author is without a receipt, the temporary row expires with this
    /// receipt and the author gets `burn_on_read_all_revealed`.
    async fn update_temporary_post_if_all_read(
        &self,
        post: &Post,
        receipt: &ReadReceipt,
    ) -> AppResult<()> {
        let unread = self
            .store()
            .read_receipt()
            .get_unread_count_for_post(&post.id, &post.channel_id, &post.user_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "unread count failed");
                app_error(
                    "RevealPost",
                    "app.reveal_post.read_receipt.get_unread_count.error",
                    500,
                )
            })?;
        if unread > 0 {
            return Ok(());
        }

        let mut temporary = self
            .store()
            .temporary_post()
            .get(&post.id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "temporary post lookup failed");
                app_error("RevealPost", "app.post.get_post.app_error", 500)
            })?;
        temporary.expire_at = receipt.expire_at;
        self.store()
            .temporary_post()
            .save(&temporary)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "temporary post write failed");
                app_error("RevealPost", "app.post.get_post.app_error", 500)
            })?;

        let mut event = WebSocketEvent::new(
            WEBSOCKET_EVENT_BURN_ON_READ_ALL_REVEALED,
            "",
            &post.channel_id,
            &post.user_id,
            None,
            "",
        );
        event.add("post_id", serde_json::Value::String(post.id.clone()));
        event.add("sender_expire_at", serde_json::json!(receipt.expire_at));
        self.publish(event).await;
        Ok(())
    }

    /// Port of `app.App.BurnPost` (app/post.go:4125) for a reader who is not the author.
    ///
    /// The author's arm is `PermanentDeletePostDataRetainStub` (app/content_flagging.go:687)
    /// — files, edit history, notifications, acknowledgements, priority, reminders and the row
    /// itself, each step reported — and is not ported; it is refused **before** anything is
    /// read, so the caller forwards it. For anyone else: no receipt is the 400
    /// `app.burn_post.not_revealed.app_error`; an expired receipt is a **no-op 200**; a live
    /// one is expired now and the reader's other clients get `post_burned`.
    #[tracing::instrument(skip(self, post), fields(post_id = %post.id, user_id = %user_id))]
    pub async fn burn_post(
        &self,
        post: &Post,
        user_id: &str,
        connection_id: &str,
    ) -> Result<(), PrepareError> {
        if post.post_type != POST_TYPE_BURN_ON_READ {
            return Err(PrepareError::App(app_error(
                "BurnPost",
                "app.burn_post.not_burn_on_read.app_error",
                400,
            )));
        }
        if post.user_id == user_id {
            return Err(PrepareError::Unreproducible(
                "the author's burn is PermanentDeletePostDataRetainStub",
            ));
        }

        let mut receipt = match self.store().read_receipt().get(&post.id, user_id).await {
            Ok(receipt) => receipt,
            Err(err) if err.is_not_found() => {
                return Err(PrepareError::App(app_error(
                    "BurnPost",
                    "app.burn_post.not_revealed.app_error",
                    400,
                )));
            }
            Err(err) => {
                tracing::error!(error = %err, "read receipt lookup failed");
                return Err(PrepareError::App(app_error(
                    "BurnPost",
                    "app.burn_post.read_receipt.get.error",
                    500,
                )));
            }
        };

        let current_time = get_millis();
        if receipt.expire_at < current_time {
            return Ok(());
        }
        receipt.expire_at = current_time;
        self.store()
            .read_receipt()
            .update(&receipt)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "read receipt update failed");
                app_error("BurnPost", "app.burn_post.read_receipt.update.error", 500)
            })?;

        let mut event = WebSocketEvent::new(
            WEBSOCKET_EVENT_POST_BURNED,
            "",
            &post.channel_id,
            user_id,
            None,
            connection_id,
        );
        event.add("post_id", serde_json::Value::String(post.id.clone()));
        self.publish(event).await;
        Ok(())
    }

    /// Port of `app.App.RewriteMessage` (app/post.go:3469) up to the agents bridge. What is
    /// reproduced is every refusal ahead of the bridge call: the thread-context reads for a
    /// `root_id` (`GetPostIfAuthorized`, whose 403/404 are the route's), and the empty prompt
    /// — an unknown `action` with a non-empty message — as the 400
    /// `app.post.rewrite.invalid_action`. The bridge itself (`agentsBridge.AgentCompletion`,
    /// a request into the AI plugin) is not reproduced; on this stack it answers the 500
    /// `app.post.rewrite.agent_call_failed`, and the caller forwards so that a stack with the
    /// plugin gets the plugin's answer.
    ///
    /// `GetPostThread` after the authorization read is not repeated: on a post that was just
    /// read it can only fail on the database, and its result is only ever folded into the
    /// prompt text the bridge sees.
    #[tracing::instrument(skip(self, session), fields(root_id = %root_id, action = %action))]
    pub async fn rewrite_message_gates(
        &self,
        session: &Session,
        message: &str,
        action: &str,
        root_id: &str,
    ) -> Result<(), PrepareError> {
        if !root_id.is_empty() {
            self.get_post_if_authorized(root_id, session, false).await?;
        }
        if !mm_model::post_rest::rewrite_action_is_accepted(action, message) {
            return Err(PrepareError::App(AppError::boxed(
                "RewriteMessage",
                "app.post.rewrite.invalid_action",
                None,
                format!("invalid action: {action}"),
                400,
            )));
        }
        Err(PrepareError::Unreproducible(
            "the agents bridge is a request into the AI plugin",
        ))
    }

    /// Port of `app.App.GetPostsForReporting` (app/report.go:313): the Enterprise licence
    /// gate (400 `license_error`), the store page, and — on `include_metadata` — each post
    /// through `PreparePostForClient` (no embeds, no images) with `IncludePriority`.
    #[tracing::instrument(skip(self, params), fields(channel_id = %params.channel_id, include_metadata, found))]
    pub async fn get_posts_for_reporting(
        &self,
        params: &ReportPostQueryParams,
        include_metadata: bool,
    ) -> Result<ReportPostListResponse, PrepareError> {
        let license = self.license().await?;
        if !minimum_enterprise_license(license.as_deref()) {
            return Err(PrepareError::App(app_error(
                "GetPostsForReporting",
                "app.post.get_posts_for_reporting.license_error",
                400,
            )));
        }

        let mut response = self
            .store()
            .post()
            .get_posts_for_reporting(params)
            .await
            .map_err(|err| {
                if err.is_invalid_input() {
                    app_error(
                        "GetPostsForReporting",
                        "app.post.get_posts_for_reporting.invalid_input_error",
                        400,
                    )
                } else {
                    tracing::error!(error = %err, "posts for reporting failed");
                    app_error(
                        "GetPostsForReporting",
                        "app.post.get_posts_for_reporting.app_error",
                        500,
                    )
                }
            })?;
        tracing::Span::current().record("found", response.posts.len());

        if include_metadata {
            let mut enriched = Vec::with_capacity(response.posts.len());
            for post in &response.posts {
                enriched.push(
                    self.prepare_post_for_client(
                        post,
                        PreparePostForClientOpts {
                            include_priority: true,
                            ..PreparePostForClientOpts::default()
                        },
                    )
                    .await?,
                );
            }
            response.posts = enriched;
        }
        Ok(response)
    }

    /// Port of `app.App.StartUsersBatchExport` (app/report.go:203): the Professional licence
    /// gate (400 `license_error`), the nine-key job data, `checkForExistingJobs` over the
    /// pending and then the in-progress jobs of the type (400 `job_exists` on a match of the
    /// seven filter keys), `CreateJob`, and then — off the request, as Go's `Srv().Go` — a DM
    /// from the system bot telling the requester the export started.
    ///
    /// The DM's text is `i18n.GetUserTranslations(user.Locale)` of
    /// `app.report.start_users_batch_export.started_export`, with the range from the **server**
    /// locale (`getTranslatedDateRange` uses `i18n.T`). English on both counts here, as every
    /// other server-authored post on this server is; a requester with another locale reads a
    /// different sentence from Go.
    #[tracing::instrument(skip(self, session, options), fields(date_range = %options.base.date_range))]
    pub async fn start_users_batch_export(
        &self,
        session: &Session,
        options: &UserReportOptions,
        start_at: i64,
        end_at: i64,
    ) -> AppResult<()> {
        let license = self.license().await?;
        if !minimum_professional_license(license.as_deref()) {
            return Err(app_error(
                "StartUsersBatchExport",
                "app.report.start_users_batch_export.license_error",
                400,
            ));
        }

        let mut data = StringMap::new();
        data.insert("requesting_user_id".to_owned(), session.user_id.clone());
        data.insert("date_range".to_owned(), options.base.date_range.clone());
        data.insert("role".to_owned(), options.role.clone());
        data.insert("team".to_owned(), options.team.clone());
        data.insert("hide_active".to_owned(), options.hide_active.to_string());
        data.insert(
            "hide_inactive".to_owned(),
            options.hide_inactive.to_string(),
        );
        data.insert("start_at".to_owned(), start_at.to_string());
        data.insert("end_at".to_owned(), end_at.to_string());
        data.insert("guest_filter".to_owned(), options.guest_filter.clone());

        self.check_for_existing_jobs(&data, JOB_TYPE_EXPORT_USERS_TO_CSV)
            .await?;
        self.create_job(JOB_TYPE_EXPORT_USERS_TO_CSV, Some(data))
            .await?;

        let app = self.clone();
        let session = session.clone();
        let date_range = options.base.date_range.clone();
        tokio::spawn(async move {
            app.post_batch_export_started(&session, &date_range).await;
        });
        Ok(())
    }

    /// Port of `app.App.checkForExistingJobs` (app/report.go:265). The two status reads are
    /// separate and in this order; a store failure is the job server's 500.
    async fn check_for_existing_jobs(&self, options: &StringMap, job_type: &str) -> AppResult<()> {
        const KEYS: [&str; 7] = [
            "date_range",
            "requesting_user_id",
            "role",
            "team",
            "hide_active",
            "hide_inactive",
            "guest_filter",
        ];
        for status in [JOB_STATUS_PENDING, JOB_STATUS_IN_PROGRESS] {
            let jobs = self
                .store()
                .job()
                .get_all_by_type_and_status(job_type, status)
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, status, "jobs by type and status failed");
                    app_error("GetJobsByTypeAndStatus", "app.job.get_all.app_error", 500)
                })?;
            let exists = jobs.iter().any(|job| {
                // `job.Data["k"]` on a nil map is `""`, so a job with no data matches the
                // options only when every one of the seven is empty.
                KEYS.iter().all(|key| {
                    job.data
                        .as_ref()
                        .and_then(|data| data.get(*key))
                        .map(String::as_str)
                        .unwrap_or("")
                        == options.get(*key).map(String::as_str).unwrap_or("")
                })
            });
            if exists {
                return Err(app_error(
                    "StartUsersBatchExport",
                    "app.report.start_users_batch_export.job_exists",
                    400,
                ));
            }
        }
        Ok(())
    }

    /// The goroutine of `StartUsersBatchExport`: system bot, DM, requester, one post. Every
    /// failure is logged and nothing is retried.
    async fn post_batch_export_started(&self, session: &Session, date_range: &str) {
        let bot = match self.get_system_bot().await {
            Ok(bot) => bot,
            Err(err) => {
                tracing::error!(error = %err, "Failed to get the system bot");
                return;
            }
        };
        let channel = match self
            .get_or_create_direct_channel(&session.user_id, &bot.user_id)
            .await
        {
            Ok(crate::channel_create::ChannelCreate::Created(channel)) => channel,
            Ok(crate::channel_create::ChannelCreate::Forward(why)) => {
                tracing::error!(reason = why, "Failed to get or create the DM");
                return;
            }
            Err(err) => {
                tracing::error!(error = %err, "Failed to get or create the DM");
                return;
            }
        };
        if let Err(err) = self.get_user(&session.user_id).await {
            tracing::error!(error = %err, "Failed to get the user");
            return;
        }
        let range = match date_range {
            REPORT_DURATION_LAST_30_DAYS => "the last 30 days",
            REPORT_DURATION_PREVIOUS_MONTH => "the previous month",
            REPORT_DURATION_LAST_6_MONTHS => "the last 6 months",
            _ => "all time",
        };
        let post = Post {
            channel_id: channel.id.clone(),
            message: format!(
                "You've started an export of user data for {range}. When the export is complete, a CSV file will be delivered to you in this direct message."
            ),
            user_id: bot.user_id.clone(),
            ..Post::default()
        };
        if let Err(err) = self
            .create_post(
                post,
                &channel,
                session,
                CreatePostFlags {
                    set_online: true,
                    ..CreatePostFlags::default()
                },
            )
            .await
        {
            tracing::error!(error = %err, "Failed to post batch export message");
        }
    }

    /// Port of `app.App.OpenInteractiveDialog` (app/integration_action.go:309): verify the
    /// trigger id against the installation's signing key within
    /// `OutgoingIntegrationRequestsTimeout`, swap the signed id for the client one, warn (only)
    /// when the dialog is invalid, and push the whole request as the `dialog` string of an
    /// `open_dialog` event to the user the trigger was minted for.
    #[tracing::instrument(skip_all, fields(url = %request.url))]
    pub async fn open_interactive_dialog(&self, mut request: OpenDialogRequest) -> AppResult<()> {
        let timeout = Duration::from_secs(
            u64::try_from(self.config().outgoing_integration_requests_timeout).unwrap_or(0),
        );
        let key = self.asymmetric_signing_verifying_key().await;
        let (client_trigger_id, user_id) =
            decode_and_verify_trigger_id(&request.trigger_id, key.as_ref(), timeout)?;
        request.trigger_id = client_trigger_id;

        if let Err(err) = request.is_valid() {
            tracing::warn!(error = %err, "Interactive dialog is invalid");
        }

        let encoded = match go_json_marshal(&request) {
            Ok(encoded) => encoded,
            Err(err) => {
                // Go logs and publishes the empty string.
                tracing::warn!(error = %err, "Error encoding request");
                String::new()
            }
        };
        let mut event =
            WebSocketEvent::new(WEBSOCKET_EVENT_OPEN_DIALOG, "", "", &user_id, None, "");
        event.add("dialog", serde_json::Value::String(encoded));
        self.publish(event).await;
        Ok(())
    }

    /// Port of `app.App.SubmitInteractiveDialog` (app/integration_action.go:334) up to the
    /// outbound `POST`: the declared file ids (deduplicated in order, at most ten, each an
    /// existing file the submitter owns) and the defence-in-depth scan of the submission for
    /// id-shaped tokens that turn out to be files. Returns the disposition of the URL.
    ///
    /// The scan's store failure is a warning in Go and the submission proceeds; the declared
    /// ids' store failure is the 500. `MaxDialogSubmissionIDShapedTokenScan` bounds the
    /// **candidates**, and hitting it is the 400 `too_many_submission_ids` — a padded
    /// submission cannot smuggle an unchecked id past the cap.
    #[tracing::instrument(skip_all, fields(url = %request.url, user_id = %request.user_id))]
    pub async fn submit_interactive_dialog_gates(
        &self,
        request: &SubmitDialogRequest,
    ) -> AppResult<OutboundDisposition> {
        let file_ids = remove_duplicate_strings_non_sort(&request.file_ids);
        if file_ids.len() > MAX_DIALOG_FILE_IDS {
            return Err(app_error(
                "SubmitInteractiveDialog",
                "app.submit_interactive_dialog.too_many_file_ids",
                400,
            ));
        }
        if !file_ids.is_empty() {
            let declared = self
                .store()
                .file_info()
                .get_by_ids(&file_ids, false)
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, "declared file lookup failed");
                    app_error(
                        "SubmitInteractiveDialog",
                        "app.submit_interactive_dialog.get_file_info_error",
                        500,
                    )
                })?;
            for info in &declared {
                if info.creator_id != request.user_id {
                    return Err(app_error(
                        "SubmitInteractiveDialog",
                        "app.submit_interactive_dialog.file_not_owned",
                        403,
                    ));
                }
            }
            for id in &file_ids {
                if !declared.iter().any(|info| &info.id == id) {
                    return Err(app_error(
                        "SubmitInteractiveDialog",
                        "app.submit_interactive_dialog.invalid_file_id",
                        400,
                    ));
                }
            }
        }

        let mut candidates: Vec<String> = Vec::new();
        let mut scan_limit_exceeded = false;
        if let Some(submission) = &request.submission {
            for value in submission.values() {
                if scan_limit_exceeded {
                    break;
                }
                collect_id_shaped_tokens(
                    value,
                    0,
                    &file_ids,
                    &mut candidates,
                    &mut scan_limit_exceeded,
                );
            }
        }
        if scan_limit_exceeded {
            return Err(app_error(
                "SubmitInteractiveDialog",
                "app.submit_interactive_dialog.too_many_submission_ids",
                400,
            ));
        }
        if !candidates.is_empty() {
            match self
                .store()
                .file_info()
                .get_by_ids(&candidates, false)
                .await
            {
                Err(err) => {
                    tracing::warn!(error = %err, "Could not resolve submission file IDs for ownership check");
                }
                Ok(found) => {
                    for info in &found {
                        if info.creator_id != request.user_id {
                            return Err(app_error(
                                "SubmitInteractiveDialog",
                                "app.submit_interactive_dialog.file_not_owned",
                                403,
                            ));
                        }
                    }
                    if file_ids.len() + found.len() > MAX_DIALOG_FILE_IDS {
                        return Err(app_error(
                            "SubmitInteractiveDialog",
                            "app.submit_interactive_dialog.too_many_file_ids",
                            400,
                        ));
                    }
                }
            }
        }

        Ok(self.outbound_disposition(&request.url).await)
    }

    /// Port of `app.App.ExecuteDialogAction` (app/integration_action.go:527) up to the
    /// outbound `POST`: the URL (again), `ValidateActionQuery` on the context, and the three
    /// identity reads — the user (500), the channel (its own error), the team when the channel
    /// has one (500) — whose values only the forwarded request uses.
    #[tracing::instrument(skip_all, fields(url = %url, channel_id = %channel_id))]
    pub async fn execute_dialog_action_gates(
        &self,
        user_id: &str,
        url: &str,
        context: &StringMap,
        channel_id: &str,
        team_id: &str,
    ) -> AppResult<OutboundDisposition> {
        if !mm_model::integration_action::is_valid_lookup_url(url) {
            return Err(AppError::boxed(
                "ExecuteDialogAction",
                "api.post.do_action.action_integration.app_error",
                None,
                "invalid URL".to_owned(),
                400,
            ));
        }
        if let Err(err) = validate_action_query(context) {
            return Err(AppError::boxed(
                "ExecuteDialogAction",
                "api.post.do_action.action_integration.app_error",
                None,
                err.to_string(),
                400,
            ));
        }
        self.store().user().get(user_id).await.map_err(|err| {
            tracing::error!(error = %err, "user lookup failed");
            app_error("ExecuteDialogAction", "app.user.get.app_error", 500)
        })?;
        self.get_channel(channel_id).await?;
        if !team_id.is_empty() {
            self.store().team().get(team_id).await.map_err(|err| {
                tracing::error!(error = %err, "team lookup failed");
                app_error("ExecuteDialogAction", "app.team.get.finding.app_error", 500)
            })?;
        }
        Ok(self.outbound_disposition(url).await)
    }

    /// Port of `app.App.LookupInteractiveDialog` (app/integration_action.go:607) up to the
    /// outbound `POST`, which is its first observable step.
    pub async fn lookup_interactive_dialog_gates(&self, url: &str) -> OutboundDisposition {
        self.outbound_disposition(url).await
    }

    /// Port of `app.App.DoPostActionWithCookie` (app/integration_action.go:41) and
    /// `resolvePostActionSetupFromPost` (app/integration_action_setup.go:325) for a request
    /// **without** a cookie, up to the integration call.
    ///
    /// `ValidateActionQuery` first (400 `api.post.do_action.query.app_error`); the post, whose
    /// absence with no cookie is the 404 `app.post.get.app_error`; its channel (500
    /// `app.channel.get_for_post.app_error`); then the client's `integration_format`,
    /// normalised: `mm_block`, `block` and `card` need `FeatureFlags.MmBlocksEnabled` (400
    /// `action_integration` when off) and resolve the action from `props.mm_blocks_actions` —
    /// unknown is the 404 `api.post.do_action.action_id.app_error`, an `openURL` action is a
    /// `goto_location` with no request at all, an `external` one names its URL; anything else
    /// is `attachment`, whose action comes from `props.attachments[].actions[]` and is the
    /// same 404 when missing or without an `integration`.
    ///
    /// The identity reads `finishPostActionSetup` makes after this (the user, the team) and
    /// the trigger id it signs only feed the forwarded request.
    #[tracing::instrument(skip(self, client_query), fields(post_id = %post_id, action_id = %action_id, integration_format = %integration_format))]
    pub async fn do_post_action_gates(
        &self,
        post_id: &str,
        action_id: &str,
        client_query: &StringMap,
        integration_format: &str,
    ) -> AppResult<PostActionOutcome> {
        use mm_model::integration_action::{
            POST_ACTION_INTEGRATION_FORMAT_BLOCK, POST_ACTION_INTEGRATION_FORMAT_CARD,
            POST_ACTION_INTEGRATION_FORMAT_MM_BLOCK, normalize_post_action_integration_format,
        };
        use mm_model::mm_blocks_actions::{MmBlocksActionError, resolve_mm_blocks_action};
        use mm_store::ChannelStore as _;

        if validate_action_query(client_query).is_err() {
            return Err(app_error(
                "DoPostActionWithCookie",
                "api.post.do_action.query.app_error",
                400,
            ));
        }

        let post = self
            .store()
            .post()
            .get_single(post_id, false)
            .await
            .map_err(|err| {
                let status = if err.is_not_found() { 404 } else { 500 };
                app_error("DoPostActionWithCookie", "app.post.get.app_error", status)
            })?;
        self.store()
            .channel()
            .get_for_post(post_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "channel for post lookup failed");
                app_error(
                    "DoPostActionWithCookie",
                    "app.channel.get_for_post.app_error",
                    500,
                )
            })?;

        let action_integration = |details: String| {
            AppError::boxed(
                "DoPostActionWithCookie",
                "api.post.do_action.action_integration.app_error",
                None,
                details,
                400,
            )
        };
        let not_found = |details: String| {
            AppError::boxed(
                "DoPostActionWithCookie",
                "api.post.do_action.action_id.app_error",
                None,
                details,
                404,
            )
        };

        let url = match normalize_post_action_integration_format(integration_format) {
            POST_ACTION_INTEGRATION_FORMAT_MM_BLOCK
            | POST_ACTION_INTEGRATION_FORMAT_BLOCK
            | POST_ACTION_INTEGRATION_FORMAT_CARD => {
                if !self.config().feature_flag_mm_blocks_enabled {
                    return Err(action_integration("mm_blocks are not enabled".to_owned()));
                }
                let spec = post.get_mm_blocks_action_spec(action_id);
                let resolved =
                    match resolve_mm_blocks_action(spec.as_ref(), action_id, client_query) {
                        Ok(resolved) => resolved,
                        Err(MmBlocksActionError::NotFound(_)) => {
                            return Err(not_found(format!("mm_blocks action_id={action_id}")));
                        }
                        Err(err) => return Err(action_integration(err.to_string())),
                    };
                if !resolved.open_url_goto.is_empty() {
                    return Ok(PostActionOutcome::Goto(resolved.open_url_goto));
                }
                resolved.external_url
            }
            _ => {
                let Some(integration) = post
                    .get_action(action_id)
                    .and_then(|action| action.integration)
                else {
                    return Err(not_found(format!("action={action_id}")));
                };
                integration.url
            }
        };

        Ok(PostActionOutcome::Outbound(
            self.outbound_disposition(&url).await,
        ))
    }

    /// What `DoActionRequest` (app/integration_action.go:135) and `getPostActionClient` (:180)
    /// would do with `raw_url`, without dialling: see [`OutboundDisposition`].
    ///
    /// `path.Clean(rawURL)` with a `/plugins/` or `plugins/` prefix is the plugin host
    /// (`DoLocalRequest`); a URL whose host is the site's and whose path starts under
    /// `{subpath}/plugins` is sent with the session token and **no** guard; anything else goes
    /// through `MakeClient(false)`, whose dialer refuses reserved and own addresses unless
    /// `AllowedUntrustedInternalConnections` allows them. A URL that will not parse, or a name
    /// that will not resolve, fails in the same `httpClient.Do` and is the same 400.
    pub async fn outbound_disposition(&self, raw_url: &str) -> OutboundDisposition {
        let cleaned = mm_model::go_path::clean(raw_url);
        if cleaned.starts_with("/plugins/") || cleaned.starts_with("plugins/") {
            return OutboundDisposition::Forward("a /plugins/ path is served by the plugin host");
        }

        let Ok(parsed) = reqwest::Url::parse(raw_url) else {
            return OutboundDisposition::Refused;
        };
        let Some(host) = parsed.host_str() else {
            return OutboundDisposition::Refused;
        };
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return OutboundDisposition::Refused;
        }

        let site_url = self.config().site_url.clone().unwrap_or_default();
        if let Ok(site) = reqwest::Url::parse(&site_url) {
            let subpath = site.path().trim_end_matches('/');
            let plugins_prefix = format!("{subpath}/plugins");
            if site.host_str() == Some(host)
                && mm_model::go_path::clean(parsed.path()).starts_with(&plugins_prefix)
            {
                return OutboundDisposition::Forward(
                    "the site's own /plugins/ subtree is called with the session token",
                );
            }
        }

        let Some(port) = parsed.port_or_known_default() else {
            return OutboundDisposition::Refused;
        };
        let guard = GuardedClient::new(
            &self.config().allowed_untrusted_internal_connections,
            self.config().enable_insecure_outgoing_connections,
        );
        match guard.permits(host, port).await {
            Ok(()) => {
                OutboundDisposition::Forward("the integration is called and its answer processed")
            }
            Err(GuardError::Forbidden(reasons)) => {
                tracing::debug!(
                    host,
                    reasons,
                    "the outbound guard refuses the integration URL"
                );
                OutboundDisposition::Refused
            }
            Err(err) => {
                tracing::debug!(host, error = %err, "the integration host does not resolve");
                OutboundDisposition::Refused
            }
        }
    }

    /// The P-256 public half of `Systems.AsymmetricSigningKey`, as `ecdsa.Verify` needs it.
    /// `None` — no row, another curve, a coordinate that does not fit — is what a Go server
    /// could not have started with, and every trigger id then fails verification.
    pub(crate) async fn asymmetric_signing_verifying_key(
        &self,
    ) -> Option<p256::ecdsa::VerifyingKey> {
        let row = self
            .system_value(crate::config::SYSTEM_ASYMMETRIC_SIGNING_KEY)
            .await?;
        crate::config::asymmetric_signing_verifying_key(&row)
    }
}

/// Port of `model.DecodeAndVerifyTriggerId` (integration_action.go:664): base64 (standard,
/// padded), four colon-separated parts, the millisecond timestamp against `timeout`, the
/// signature's base64, its ASN.1, and finally `ecdsa.Verify` over SHA-256 of the first three
/// parts joined with a trailing colon. Returns `(client_trigger_id, user_id)`.
///
/// Six 400s in that order. The timestamp parse error is **discarded** (`strconv.ParseInt`'s
/// result is used as zero), so a non-numeric timestamp is simply "expired".
pub fn decode_and_verify_trigger_id(
    trigger_id: &str,
    key: Option<&p256::ecdsa::VerifyingKey>,
    timeout: Duration,
) -> AppResult<(String, String)> {
    use base64::Engine as _;
    use p256::ecdsa::signature::Verifier as _;

    let invalid =
        |id: &str, params: Option<std::collections::HashMap<String, serde_json::Value>>| {
            AppError::boxed("DecodeAndVerifyTriggerId", id, params, String::new(), 400)
        };
    let stripped: String = trigger_id
        .chars()
        .filter(|c| *c != '\r' && *c != '\n')
        .collect();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(stripped.as_bytes())
        .map_err(|_| {
            invalid(
                "interactive_message.decode_trigger_id.base64_decode_failed",
                None,
            )
        })?;
    let decoded = String::from_utf8_lossy(&decoded);
    let split: Vec<&str> = decoded.split(':').collect();
    if split.len() != 4 {
        return Err(invalid(
            "interactive_message.decode_trigger_id.missing_data",
            None,
        ));
    }
    let client_trigger_id = split[0];
    let user_id = split[1];
    let timestamp_str = split[2];
    let timestamp: i64 = timestamp_str.parse().unwrap_or(0);

    // `time.Since(time.UnixMilli(timestamp)) > timeout`. A timestamp in the future is a
    // negative age, never expired.
    let age_millis = get_millis().saturating_sub(timestamp);
    if age_millis > i64::try_from(timeout.as_millis()).unwrap_or(i64::MAX) {
        let mut params = std::collections::HashMap::new();
        params.insert(
            "Duration".to_owned(),
            serde_json::Value::String(go_duration_string(timeout)),
        );
        return Err(invalid(
            "interactive_message.decode_trigger_id.expired",
            Some(params),
        ));
    }

    let signature = base64::engine::general_purpose::STANDARD
        .decode(split[3].as_bytes())
        .map_err(|_| {
            invalid(
                "interactive_message.decode_trigger_id.base64_decode_failed_signature",
                None,
            )
        })?;
    let signature = p256::ecdsa::Signature::from_der(&signature).map_err(|_| {
        invalid(
            "interactive_message.decode_trigger_id.signature_decode_failed",
            None,
        )
    })?;

    let trigger_data = format!("{client_trigger_id}:{user_id}:{timestamp_str}:");
    let verified = key.is_some_and(|key| key.verify(trigger_data.as_bytes(), &signature).is_ok());
    if !verified {
        return Err(invalid(
            "interactive_message.decode_trigger_id.verify_signature_failed",
            None,
        ));
    }
    Ok((client_trigger_id.to_owned(), user_id.to_owned()))
}

/// `time.Duration.String()` for the whole-second durations the config can hold: `30s`,
/// `1m30s`, `2h0m0s`. Only ever reaches the `Duration` param of the expired error, which the
/// translated message would interpolate.
fn go_duration_string(duration: Duration) -> String {
    let secs = duration.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m}m{s}s")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

/// Port of `extractPostExpiration` (app/post.go:3818): the `expire_at` prop as an integer —
/// Go accepts `int64` and `float64` (a JSON number), and anything else, absent, or zero is the
/// 400 `missing_expire_at`. A float is truncated, as `int64(v)` is.
fn extract_post_expiration(post: &Post) -> Result<i64, PrepareError> {
    let missing = || {
        PrepareError::App(app_error(
            "RevealPost",
            "app.reveal_post.missing_expire_at.app_error",
            400,
        ))
    };
    let expire_at = match post.get_prop(POST_PROPS_EXPIRE_AT) {
        Some(serde_json::Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .ok_or_else(missing)?,
        _ => return Err(missing()),
    };
    if expire_at == 0 {
        return Err(missing());
    }
    Ok(expire_at)
}

/// The `collectIDs` closure of `SubmitInteractiveDialog`: comma-split strings, recursing into
/// arrays and objects to depth 100, collecting id-shaped tokens that are neither declared nor
/// already seen, and flagging the cap rather than silently stopping.
fn collect_id_shaped_tokens(
    value: &serde_json::Value,
    depth: usize,
    declared: &[String],
    candidates: &mut Vec<String>,
    limit_exceeded: &mut bool,
) {
    const MAX_DEPTH: usize = 100;
    if depth > MAX_DEPTH || *limit_exceeded {
        return;
    }
    match value {
        serde_json::Value::String(text) => {
            for token in text.split(',') {
                let token = token.trim();
                if token.is_empty()
                    || declared.iter().any(|d| d == token)
                    || candidates.iter().any(|c| c == token)
                    || !is_valid_id(token)
                {
                    continue;
                }
                if candidates.len() >= MAX_DIALOG_SUBMISSION_ID_SHAPED_TOKEN_SCAN {
                    *limit_exceeded = true;
                    return;
                }
                candidates.push(token.to_owned());
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                if *limit_exceeded {
                    return;
                }
                collect_id_shaped_tokens(item, depth + 1, declared, candidates, limit_exceeded);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values() {
                if *limit_exceeded {
                    return;
                }
                collect_id_shaped_tokens(item, depth + 1, declared, candidates, limit_exceeded);
            }
        }
        _ => {}
    }
}

impl From<StoreError> for PrepareError {
    fn from(err: StoreError) -> Self {
        PrepareError::App(AppError::boxed(
            "store",
            "app.store.app_error",
            None,
            err.to_string(),
            500,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc822_is_gos_layout_in_utc() {
        assert_eq!(rfc822_utc(4_102_444_800), "01 Jan 00 00:00 UTC");
        assert_eq!(rfc822_utc(1_700_000_000), "14 Nov 23 22:13 UTC");
    }

    #[test]
    fn go_durations_print_as_go_prints_them() {
        assert_eq!(go_duration_string(Duration::from_secs(30)), "30s");
        assert_eq!(go_duration_string(Duration::from_secs(90)), "1m30s");
        assert_eq!(go_duration_string(Duration::from_secs(7200)), "2h0m0s");
    }

    #[test]
    fn the_expire_at_prop_accepts_numbers_only() {
        let mut post = Post::default();
        assert!(extract_post_expiration(&post).is_err());
        post.add_prop(POST_PROPS_EXPIRE_AT, serde_json::json!(0));
        assert!(extract_post_expiration(&post).is_err());
        post.add_prop(POST_PROPS_EXPIRE_AT, serde_json::json!("12"));
        assert!(extract_post_expiration(&post).is_err());
        post.add_prop(POST_PROPS_EXPIRE_AT, serde_json::json!(1234.9));
        assert_eq!(extract_post_expiration(&post).unwrap(), 1234);
    }

    #[test]
    fn the_trigger_id_refusals_come_in_gos_order() {
        use base64::Engine as _;
        let timeout = Duration::from_secs(30);
        let id = |raw: &str| {
            decode_and_verify_trigger_id(raw, None, timeout)
                .unwrap_err()
                .id
        };
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
        assert_eq!(
            id("***"),
            "interactive_message.decode_trigger_id.base64_decode_failed"
        );
        assert_eq!(
            id(&b64("a:b:c")),
            "interactive_message.decode_trigger_id.missing_data"
        );
        assert_eq!(
            id(&b64("a:b:0:sig")),
            "interactive_message.decode_trigger_id.expired"
        );
        assert_eq!(
            id(&b64("a:b:notanumber:sig")),
            "interactive_message.decode_trigger_id.expired"
        );
        let now = get_millis();
        assert_eq!(
            id(&b64(&format!("a:b:{now}:***"))),
            "interactive_message.decode_trigger_id.base64_decode_failed_signature"
        );
        assert_eq!(
            id(&b64(&format!("a:b:{now}:{}", b64("not der")))),
            "interactive_message.decode_trigger_id.signature_decode_failed"
        );
        // A well-formed DER signature over the wrong key (none here) fails verification.
        let signing = p256::ecdsa::SigningKey::from_slice(&[7u8; 32]).unwrap();
        use p256::ecdsa::signature::Signer as _;
        let sig: p256::ecdsa::Signature = signing.sign(format!("a:b:{now}:").as_bytes());
        let der = base64::engine::general_purpose::STANDARD.encode(sig.to_der().as_bytes());
        assert_eq!(
            id(&b64(&format!("a:b:{now}:{der}"))),
            "interactive_message.decode_trigger_id.verify_signature_failed"
        );
        // And with the right key it verifies, returning the client id and the user.
        let verifying = p256::ecdsa::VerifyingKey::from(&signing);
        assert_eq!(
            decode_and_verify_trigger_id(
                &b64(&format!("a:b:{now}:{der}")),
                Some(&verifying),
                timeout
            )
            .unwrap(),
            ("a".to_owned(), "b".to_owned())
        );
    }

    #[test]
    fn the_submission_scan_collects_id_shaped_tokens_and_caps_them() {
        let declared = vec!["abcdefghijklmnopqrstuvwxyz".to_owned()];
        let mut found = Vec::new();
        let mut exceeded = false;
        let value = serde_json::json!({
            "a": "abcdefghijklmnopqrstuvwxyz, zyxwvutsrqponmlkjihgfedcba ,short",
            "b": ["zyxwvutsrqponmlkjihgfedcba", {"c": "aaaaaaaaaaaaaaaaaaaaaaaaaa"}],
            "d": 7
        });
        collect_id_shaped_tokens(&value, 0, &declared, &mut found, &mut exceeded);
        assert_eq!(
            found,
            vec![
                "zyxwvutsrqponmlkjihgfedcba".to_owned(),
                "aaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned()
            ]
        );
        assert!(!exceeded);

        let padded: Vec<String> = (0..MAX_DIALOG_SUBMISSION_ID_SHAPED_TOKEN_SCAN + 1)
            .map(|n| format!("{n:0>26}"))
            .collect();
        let mut found = Vec::new();
        let mut exceeded = false;
        collect_id_shaped_tokens(
            &serde_json::json!(padded),
            0,
            &[],
            &mut found,
            &mut exceeded,
        );
        assert!(exceeded);
    }
}
