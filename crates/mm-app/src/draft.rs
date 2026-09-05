//! Port of the read side of `server/channels/app/draft.go`.

use mm_model::draft::Draft;
use mm_model::post_metadata::PostMetadata;
use mm_model::utils::AppError;
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
