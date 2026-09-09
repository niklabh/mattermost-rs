//! Port of the file-backend half of `server/channels/app/export.go` — `ListExports`,
//! `DeleteExport` and `GeneratePresignURLForExport`.
//!
//! The bulk-export machinery those names suggest is not here; these three only walk a directory.
//!
//! # `ExportSettings.Directory` is not `FileSettings.ExportDirectory`
//!
//! The first is a path **inside** the export backend and defaults to `./export`. The second is
//! the export backend's **root** and defaults to `./data/`. On a stock server an export therefore
//! lives at `./data/export/<name>`, and reading either setting for the other's job puts every one
//! of these functions in the wrong directory while still finding a plausible-looking one.

use mm_model::go_path;
use mm_model::utils::AppError;

use crate::App;
use crate::post::PrepareError;

impl App {
    /// Port of `app.App.ListExports` (app/export.go:1248).
    ///
    /// Reduces each listing entry with `filepath.Base`, because `ListDirectory` returns entries
    /// prefixed with the directory it was asked about. Unlike [`App::list_imports`] it filters
    /// **nothing** — a half-written `.tmp` export is listed.
    pub async fn list_exports(&self) -> Result<Vec<String>, PrepareError> {
        let exports = self
            .list_export_directory(&self.config().export_directory)
            .await?;
        Ok(exports.iter().map(|path| go_path::base(path)).collect())
    }

    /// Port of `app.App.DeleteExport` (app/export.go:1297).
    ///
    /// **A missing export is success, not a 404.** The handler then answers `{"status":"OK"}`,
    /// so `DELETE` of a name that was never there is indistinguishable from deleting a real one.
    pub async fn delete_export(&self, name: &str) -> Result<(), PrepareError> {
        let path = go_path::join(&[&self.config().export_directory, name]);

        if !self.export_file_exists(&path).await? {
            return Ok(());
        }

        self.remove_export_file(&path).await
    }

    /// Port of `app.App.GeneratePresignURLForExport` (app/export.go:1262).
    ///
    /// **Always refuses on this deployment, at the first gate.**
    /// `FeatureFlags.EnableExportDirectDownload` defaults to `false` (feature_flags.go:168) and
    /// never reaches the persisted configuration document, so there is nothing that could turn it
    /// on for a Team Edition server. Past it there are two more refusals, and the third is
    /// structural: `GeneratePublicLink` is on `FileBackendWithLinkGenerator`, which only the S3
    /// and Azure backends implement — a local backend can never presign anything.
    ///
    /// All three ids carry Go's typo, `eport`. Reproduced: a client matching on the string sees
    /// the same one from either server.
    pub async fn generate_presign_url_for_export(
        &self,
        _name: &str,
    ) -> Result<serde_json::Value, PrepareError> {
        // Gate 1 — the feature flag, `false` here and unconfigurable.
        Err(PrepareError::App(AppError::boxed(
            "GeneratePresignURLForExport",
            "app.eport.generate_presigned_url.featureflag.app_error",
            None,
            String::new(),
            500,
        )))
    }
}
