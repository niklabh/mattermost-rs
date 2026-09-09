//! Port of the two read functions in `server/channels/app/upload.go`.
//!
//! The rest of that file is the resumable-upload flow itself — `CreateUploadSession`,
//! `UploadData` and the process-wide lock map that serialises writes to one session — none of
//! which a `GET` reaches.

use mm_model::upload_session::UploadSession;
use mm_model::utils::{AppError, AppResult};
use mm_store::UploadSessionStore;

use crate::App;

impl App {
    /// Port of `app.App.GetUploadSession` (app/upload.go:173).
    ///
    /// **One error id, two statuses.** `app.upload.get.app_error` is 404 for a miss and 500 for a
    /// query failure, so a client branching on the id cannot tell them apart — the same shape as
    /// `GetFileInfo`. The `where` is `GetUpload`, not `GetUploadSession`: Go names the *route*
    /// there and the function everywhere else in the file.
    #[tracing::instrument(skip(self), fields(upload_id = %upload_id))]
    pub async fn get_upload_session(&self, upload_id: &str) -> AppResult<UploadSession> {
        self.store()
            .upload_session()
            .get(upload_id)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    AppError::boxed(
                        "GetUpload",
                        "app.upload.get.app_error",
                        None,
                        String::new(),
                        404,
                    )
                } else {
                    tracing::error!(error = %err, "upload session lookup failed");
                    AppError::boxed(
                        "GetUpload",
                        "app.upload.get.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })
    }

    /// Port of `app.App.GetUploadSessionsForUser` (app/upload.go:189).
    ///
    /// No not-found branch at all — a user with no uploads is an empty list, not a 404 — so the
    /// single error id here is only ever a 500.
    #[tracing::instrument(skip(self), fields(user_id = %user_id))]
    pub async fn get_upload_sessions_for_user(
        &self,
        user_id: &str,
    ) -> AppResult<Vec<UploadSession>> {
        self.store()
            .upload_session()
            .get_for_user(user_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "upload session listing failed");
                AppError::boxed(
                    "GetUploadsForUser",
                    "app.upload.get_for_user.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }
}
