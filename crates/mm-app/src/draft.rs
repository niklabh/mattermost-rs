//! Port of the read side of `server/channels/app/draft.go`.

use mm_model::draft::Draft;
use mm_model::post_metadata::PostMetadata;
use mm_model::utils::{AppError, AppResult};
use mm_store::UserStore;
use mm_store::draft_store::DraftStore;
use mm_store::file_info_store::FileInfoStore;

use crate::App;
use crate::post::PrepareError;

impl App {
    /// Port of `app.App.GetDraftsForUser` (app/draft.go:102).
    ///
    /// # The feature gate is a 501, and it comes first
    ///
    /// `AllowSyncedDrafts` is checked in the handler *and* again here, and both raise **501**
    /// rather than a 404 or a 403 — before any permission check, so a caller holding nothing at
    /// all still gets the 501. The two checks carry different ids
    /// (`api.drafts.disabled.app_error` above, `app.draft.feature_disabled` here); only the
    /// handler's is reachable, because it runs first and returns.
    ///
    /// # Every draft gets a `metadata`, files or not
    ///
    /// `prepareDraftWithFileInfos` (app/draft.go:119) assigns `&model.PostMetadata{}` on the
    /// **success** path unconditionally, and `getFileInfosForDraft` returns `(nil, nil)` — not an
    /// error — for a draft with no file ids. So a plain text draft carries `"metadata": {}` on
    /// the wire: `omitempty` on a *pointer* tests the pointer, and this one is not nil. The only
    /// way to get no `metadata` key at all is the error branch, which Go logs and swallows.
    ///
    /// # A file whose preview has to be generated is not reproducible here
    ///
    /// `generateMiniPreviewForInfos` reads the file backend and writes the row back. This port
    /// has neither, so a draft holding such a file is handed to Go whole — see
    /// [`App::mini_preview_would_be_generated`], which is narrow enough that the forward is rare.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, team_id = %team_id, drafts))]
    pub async fn get_drafts_for_user(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> Result<Vec<Draft>, PrepareError> {
        if !self.config().allow_synced_drafts {
            return Err(PrepareError::App(AppError::boxed(
                "GetDraftsForUser",
                "app.draft.feature_disabled",
                None,
                String::new(),
                501,
            )));
        }

        let mut drafts = self
            .store()
            .draft()
            .get_drafts_for_user(user_id, team_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "draft lookup failed");
                PrepareError::App(AppError::boxed(
                    "GetDraftsForUser",
                    "app.draft.get_drafts.app_error",
                    None,
                    String::new(),
                    500,
                ))
            })?;
        tracing::Span::current().record("drafts", drafts.len());

        for draft in &mut drafts {
            self.prepare_draft_with_file_infos(draft).await?;
        }

        Ok(drafts)
    }

    /// Port of `app.App.prepareDraftWithFileInfos` (app/draft.go:119) and the
    /// `getFileInfosForDraft` (app/draft.go:130) it calls.
    ///
    /// # The two rejected-file rules
    ///
    /// A returned `FileInfo` is kept only when its `PostId` is **empty** — the file has not been
    /// attached to a sent post — and its `CreatorId` is the draft's own author. Anything else is
    /// logged at debug and dropped, which is what stops a client from naming somebody else's file
    /// id in a draft and reading its metadata back.
    ///
    /// # Zero surviving files is the same as no file ids
    ///
    /// Both return `(nil, nil)`, so both end at `metadata: {}`. A draft naming three ids that all
    /// fail the rules above is indistinguishable on the wire from one naming none.
    async fn prepare_draft_with_file_infos(&self, draft: &mut Draft) -> Result<(), PrepareError> {
        let ids = draft.file_ids.clone().unwrap_or_default();
        if ids.is_empty() {
            draft.metadata = Some(PostMetadata::default());
            return Ok(());
        }

        let all = match self.store().file_info().get_by_ids(&ids, false).await {
            Ok(all) => all,
            Err(err) => {
                // Go logs and leaves `Metadata` nil, so the key is absent rather than `{}`.
                tracing::error!(error = %err, "failed to get files for a user's drafts");
                return Ok(());
            }
        };

        let files: Vec<_> = all
            .into_iter()
            .filter(|info| info.post_id.is_empty() && info.creator_id == draft.user_id)
            .collect();

        if files.iter().any(App::mini_preview_would_be_generated) {
            return Err(PrepareError::Unreproducible(
                "generateMiniPreviewForInfos reads the file backend and writes the row back",
            ));
        }

        draft.metadata = Some(PostMetadata {
            files,
            ..PostMetadata::default()
        });
        Ok(())
    }
}

/// What [`App::upsert_draft`] did, or why it declined.
#[derive(Debug)]
pub enum DraftWrite {
    /// The draft was written. Carries it with `metadata` filled in, which is what the handler
    /// echoes back.
    Saved(Box<Draft>),
    /// **The message was empty, so the draft was deleted instead** and Go answers `201` with a
    /// body of `null`. See [`App::upsert_draft`].
    DeletedBecauseEmpty,
    /// A file on the draft would have had a mini-preview generated. Forward.
    Forward(&'static str),
}

impl App {
    /// Port of `app.App.UpsertDraft` (app/draft.go:36).
    ///
    /// # An empty message is a delete, and the answer is `201 null`
    ///
    /// Go returns `(nil, nil)` after deleting, and the handler writes `201 Created` and then
    /// encodes the nil pointer — so the body is the four bytes `null` with a trailing newline,
    /// at a *created* status, for a request that destroyed a row. Reproduced exactly; a port that
    /// answered 200, or `{}`, or omitted the body, would each be wrong in a different way.
    ///
    /// # Five gates, and the channel one is a 400 with a `Name` param
    ///
    /// 1. the feature gate (501, and the handler's fires first);
    /// 2. the channel must exist — **`api.context.invalid_param.app_error` at 400** with
    ///    `Name: "draft.channel_id"`, not a 404;
    /// 3. an archived channel is a 400 of its own;
    /// 4. a restricted DM is a 400 of its own;
    /// 5. the **user** must exist, and its failure is a flat 500 with no not-found arm — Go wraps
    ///    every `User().Get` error as `app.user.get.app_error` at 500 here, including the
    ///    not-found it would report as 404 anywhere else.
    ///
    /// Go fetches the channel through the **store** rather than `App.GetChannel`, so *any* store
    /// error becomes gate 2's 400 — including the not-found that `GetChannel` would report as a
    /// 404 with its own id. `Get(id, true)`'s second argument is `allowFromCache`, not
    /// include-deleted; the query does not filter `DeleteAt`, which is what makes gate 3
    /// reachable.
    #[tracing::instrument(skip(self, draft), fields(user_id = %draft.user_id, channel_id = %draft.channel_id))]
    pub async fn upsert_draft(&self, draft: &Draft, connection_id: &str) -> AppResult<DraftWrite> {
        let mut draft = draft.clone();

        let channel = self.get_channel(&draft.channel_id).await.map_err(|_| {
            let mut params: std::collections::HashMap<String, serde_json::Value> =
                std::collections::HashMap::new();
            params.insert(
                "Name".to_owned(),
                serde_json::Value::String("draft.channel_id".to_owned()),
            );
            AppError::boxed(
                "CreateDraft",
                "api.context.invalid_param.app_error",
                Some(params),
                String::new(),
                400,
            )
        })?;

        if channel.delete_at != 0 {
            return Err(AppError::boxed(
                "CreateDraft",
                "api.draft.create_draft.can_not_draft_to_deleted.error",
                None,
                String::new(),
                400,
            ));
        }

        match self.check_if_channel_is_restricted_dm(&channel).await? {
            crate::channel::RestrictedDm::No => {}
            crate::channel::RestrictedDm::Yes => {
                return Err(AppError::boxed(
                    "CreateDraft",
                    "api.draft.create_draft.can_not_draft_to_restricted_dm.error",
                    None,
                    String::new(),
                    400,
                ));
            }
            crate::channel::RestrictedDm::Undecidable => {
                return Ok(DraftWrite::Forward(
                    "a bot's exemption from DM restrictions is a plugin decision",
                ));
            }
        }

        // Go's error here has **no not-found arm**: every failure is 500.
        self.store()
            .user()
            .get(&draft.user_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "draft author lookup failed");
                AppError::boxed(
                    "CreateDraft",
                    "app.user.get.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        if draft.message.is_empty() {
            self.store()
                .draft()
                .delete(&draft.user_id, &draft.channel_id, &draft.root_id)
                .await
                .map_err(draft_save_error)?;
            tracing::debug!("Draft deleted via empty-message upsert");
            // **No websocket event.** Go returns before the publish, so a draft removed this way
            // is invisible to other sessions until they refetch — unlike `deleteDraft`, which
            // publishes `draft_deleted`.
            return Ok(DraftWrite::DeletedBecauseEmpty);
        }

        // `runGuardedDraftWillBeUpserted` is a plugin hook; with no plugin environment it is the
        // identity, so the draft goes to the store unchanged.
        draft.pre_save();

        let max_draft_size = self
            .store()
            .draft()
            .max_draft_size()
            .await
            .map_err(draft_save_error)?;
        // `IsValid` runs **inside** the store in Go, so its error reaches the handler unwrapped —
        // a message over the limit answers `model.draft.is_valid.msg.app_error` with `Length` and
        // `MaxLength` params, not `app.draft.save.app_error`.
        draft.is_valid(max_draft_size)?;

        self.store()
            .draft()
            .upsert(&draft)
            .await
            .map_err(draft_save_error)?;

        // Go re-reads nothing: the value published and returned is the struct it just saved, with
        // file infos hung off it.
        match self.prepare_draft_with_file_infos(&mut draft).await {
            Ok(()) => {}
            Err(PrepareError::Unreproducible(why)) => return Ok(DraftWrite::Forward(why)),
            Err(PrepareError::App(err)) => return Err(err),
        }

        self.publish_draft_event(
            mm_model::websocket_message::WEBSOCKET_EVENT_DRAFT_CREATED,
            &draft,
            connection_id,
        )
        .await;

        Ok(DraftWrite::Saved(Box::new(draft)))
    }

    /// Port of `app.App.DeleteDraft` (app/draft.go:158).
    ///
    /// No gates beyond the feature one: the caller has already fetched the draft (which is what
    /// produces the 404) and checked that it belongs to the session.
    #[tracing::instrument(skip(self, draft), fields(user_id = %draft.user_id, channel_id = %draft.channel_id))]
    pub async fn delete_draft(&self, draft: &Draft, connection_id: &str) -> AppResult<()> {
        self.store()
            .draft()
            .delete(&draft.user_id, &draft.channel_id, &draft.root_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "draft delete failed");
                AppError::boxed(
                    "DeleteDraft",
                    "app.draft.delete.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        self.publish_draft_event(
            mm_model::websocket_message::WEBSOCKET_EVENT_DRAFT_DELETED,
            draft,
            connection_id,
        )
        .await;

        Ok(())
    }

    /// Port of `app.App.GetDraft` (app/draft.go:17).
    ///
    /// Both arms of Go's error carry the **same id**, `app.draft.get.app_error`, and differ only
    /// in status: 404 for not-found and 500 otherwise. `deleteDraft` branches on the status, not
    /// the id, which is why the pair matters.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, channel_id = %channel_id))]
    pub async fn get_draft(
        &self,
        user_id: &str,
        channel_id: &str,
        root_id: &str,
    ) -> AppResult<Draft> {
        match self
            .store()
            .draft()
            .get(user_id, channel_id, root_id, false)
            .await
        {
            Ok(Some(draft)) => Ok(draft),
            Ok(None) => Err(AppError::boxed(
                "GetDraft",
                "app.draft.get.app_error",
                None,
                String::new(),
                404,
            )),
            Err(err) => {
                tracing::error!(error = %err, "draft lookup failed");
                Err(AppError::boxed(
                    "GetDraft",
                    "app.draft.get.app_error",
                    None,
                    String::new(),
                    500,
                ))
            }
        }
    }

    /// The two draft events share a shape: addressed to the **channel and the user both**, with
    /// the draft as a JSON string under `draft`, and omitting the originating connection.
    ///
    /// `omit_connection_id` is the client's own `X-Connection-Id` header. It is what stops the
    /// tab that saved the draft from being told about its own save — and the hub's fan-out reads
    /// it *before* `user_id`, so getting it wrong changes who is skipped rather than merely how
    /// many events go out.
    async fn publish_draft_event(&self, event: &str, draft: &Draft, connection_id: &str) {
        let mut message = mm_model::websocket_message::WebSocketEvent::new(
            event,
            "",
            &draft.channel_id,
            &draft.user_id,
            None,
            connection_id,
        );
        match serde_json::to_string(draft) {
            Ok(json) => message.add("draft", serde_json::Value::String(json)),
            Err(err) => tracing::warn!(error = %err, "Failed to encode draft to JSON"),
        }
        self.publish(message).await;
    }
}

/// `app.draft.save.app_error` at 500 — the id Go gives both the empty-message delete and the
/// upsert, though they are different operations.
fn draft_save_error(err: mm_store::StoreError) -> Box<AppError> {
    tracing::error!(error = %err, "draft save failed");
    AppError::boxed(
        "CreateDraft",
        "app.draft.save.app_error",
        None,
        String::new(),
        500,
    )
}
