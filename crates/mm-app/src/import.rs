//! Port of the file-backend half of `server/channels/app/import.go` — `ListImports` and
//! `DeleteImport`.
//!
//! The counterparts of [`crate::export`]'s two, with three differences worth naming because
//! nothing about the code says them out loud:
//!
//! 1. Imports have **no dedicated store**: both functions use the *file* backend, while the
//!    export pair uses the export backend. On a stock server those are the same object.
//! 2. `ListImports` **filters** `.tmp` entries; `ListExports` does not.
//! 3. `deleteImport`'s permission check is `SessionHasPermissionTo(PermissionManageSystem)`
//!    where `deleteExport`'s is `c.IsSystemAdmin()` — see `mm_api::exports`.

use mm_model::go_path;

use crate::App;
use crate::post::PrepareError;

/// Port of `model.IncompleteUploadSuffix` (model/upload_session.go:17).
pub const INCOMPLETE_UPLOAD_SUFFIX: &str = ".tmp";

impl App {
    /// Port of `app.App.ListImports` (app/import.go:402).
    ///
    /// The `.tmp` filter is what makes an in-progress upload invisible to the admin console. It
    /// runs on the **base name**, so a directory called `x.tmp` hides too.
    pub async fn list_imports(&self) -> Result<Vec<String>, PrepareError> {
        let imports = self.list_directory(&self.config().import_directory).await?;
        Ok(imports
            .iter()
            .map(|path| go_path::base(path))
            .filter(|name| !name.ends_with(INCOMPLETE_UPLOAD_SUFFIX))
            .collect())
    }

    /// Port of `app.App.DeleteImport` (app/import.go:419).
    ///
    /// A missing import is success, exactly as in [`App::delete_export`].
    pub async fn delete_import(&self, name: &str) -> Result<(), PrepareError> {
        let path = go_path::join(&[&self.config().import_directory, name]);

        if !self.file_exists(&path).await? {
            return Ok(());
        }

        self.remove_file(&path).await
    }
}
