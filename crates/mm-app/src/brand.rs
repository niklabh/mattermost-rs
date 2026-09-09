//! Port of `server/channels/app/brand.go` — the read and delete halves.
//!
//! Two functions over one hard-coded path, `brand/image.png`. The upload half is not here: it
//! decodes and re-encodes a PNG, and archives the previous image under a timestamped name.

use mm_model::utils::AppError;

use crate::App;
use crate::post::PrepareError;

/// Port of `app.BrandFilePath` (app/brand.go:18).
pub const BRAND_FILE_PATH: &str = "brand/";
/// Port of `app.BrandFileName` (app/brand.go:19).
pub const BRAND_FILE_NAME: &str = "image.png";

/// `BrandFilePath + BrandFileName` — Go concatenates the two at every call site rather than
/// naming the result.
fn brand_image_path() -> String {
    format!("{BRAND_FILE_PATH}{BRAND_FILE_NAME}")
}

impl App {
    /// Port of `app.App.GetBrandImage` (app/brand.go:82).
    ///
    /// # The 501 is unreachable and the 500 never reaches a client
    ///
    /// The empty-driver guard answers `api.admin.get_brand_image.storage.app_error` at 501, and
    /// `FileSettings.isValid` rejects an empty `DriverName`, so only a hand-edited configuration
    /// gets there. Everything else is `ReadFile`'s 500 — and `getBrandImage`
    /// (api4/brand.go:20) **discards whichever error it gets** and writes a bare
    /// `404` with an empty body. So the two branches below decide nothing a client can observe;
    /// they exist because the *route* has to know the difference between "no image" (404) and
    /// "not our deployment" (forward).
    pub async fn get_brand_image(&self) -> Result<Vec<u8>, PrepareError> {
        if self.config().file_driver_name.is_empty() {
            return Err(PrepareError::App(AppError::boxed(
                "GetBrandImage",
                "api.admin.get_brand_image.storage.app_error",
                None,
                String::new(),
                501,
            )));
        }

        self.read_file(&brand_image_path()).await
    }

    /// Port of `app.App.DeleteBrandImage` (app/brand.go:91).
    ///
    /// Checks existence first and answers `api.admin.delete_brand_image.storage.not_found` at 404
    /// when there is nothing to remove — unlike `DeleteExport` and `DeleteImport`, which treat a
    /// missing file as *success*. Three deletes over the same backend, two different opinions
    /// about the same situation.
    ///
    /// Note the missing empty-driver guard: `GetBrandImage` has one and this does not, so on a
    /// driverless configuration the read 501s and the delete 500s.
    pub async fn delete_brand_image(&self) -> Result<(), PrepareError> {
        let path = brand_image_path();

        if !self.file_exists(&path).await? {
            return Err(PrepareError::App(AppError::boxed(
                "DeleteBrandImage",
                "api.admin.delete_brand_image.storage.not_found",
                None,
                String::new(),
                404,
            )));
        }

        self.remove_file(&path).await
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;

    /// The path is a literal in Go and a literal here; a test is the only thing that would notice
    /// a typo, because a wrong path reads as "no brand image configured".
    #[test]
    fn the_brand_image_path_is_the_one_go_writes() {
        assert_eq!(brand_image_path(), "brand/image.png");
    }
}
