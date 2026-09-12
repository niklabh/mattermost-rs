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
    /// Port of `app.App.SaveBrandImage` (app/brand.go:22), **narrowed to the guard in front of
    /// it**.
    ///
    /// # Nothing past the guard is reproducible, and the guard is the whole point
    ///
    /// `SaveBrandImage` decodes the upload with `imgDecoder.Decode` and writes it back out with
    /// `imgEncoder.EncodePNG` — so the bytes on disk are Go's PNG encoder's, not the client's,
    /// for *every* accepted upload including one that was already a PNG. There is no
    /// write-through path here as there is for `createEmoji`: no brand image can be stored by
    /// this server and byte-compared against Go's. See [D-411].
    ///
    /// What is reproducible is the **501** in front of all of it, and reproducing it matters
    /// because `uploadBrandImage` checks `edit_brand` *before* calling this — so on a driverless
    /// server an unprivileged caller gets the 403 and a system admin gets the 501, and a port
    /// that forwarded unconditionally would leave that ordering untested.
    ///
    /// Returning before the decode also means a forwarded upload has not touched the backend:
    /// the archive-the-old-image `MoveFile` and the `WriteFile` are both past this point.
    pub async fn save_brand_image(&self) -> Result<(), PrepareError> {
        if self.config().file_driver_name.is_empty() {
            return Err(PrepareError::App(AppError::boxed(
                "SaveBrandImage",
                "api.admin.upload_brand_image.storage.app_error",
                None,
                String::new(),
                501,
            )));
        }

        Err(PrepareError::Unreproducible(
            "SaveBrandImage re-encodes the upload with Go's PNG encoder",
        ))
    }

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

    fn unreachable_store() -> mm_store::SqlStore {
        mm_store::SqlStore::from_pool(
            sqlx::postgres::PgPoolOptions::new()
                .acquire_timeout(std::time::Duration::from_millis(250))
                .connect_lazy("postgres://nobody@127.0.0.1:1/nothing")
                .expect("a lazy pool is built without connecting"),
        )
    }

    /// The 501 in front of `SaveBrandImage`, and the fact that it is not the *read*'s 501.
    ///
    /// Same setting, same handler family, three different error ids over two statuses:
    /// `GetBrandImage` raises `api.admin.get_brand_image.storage.app_error`, this raises
    /// `api.admin.upload_brand_image.storage.app_error`, and `DeleteBrandImage` has no guard at
    /// all. A test is the only way to keep the upload one from drifting onto the read one, since
    /// no client can reach either on a validly configured server.
    #[tokio::test]
    async fn a_driverless_server_refuses_the_upload_with_its_own_501() {
        let config = crate::config::Config {
            file_driver_name: String::new(),
            ..crate::config::Config::default()
        };
        let app = crate::App::with_config(unreachable_store(), config);
        let PrepareError::App(err) = app
            .save_brand_image()
            .await
            .expect_err("no file driver, no upload")
        else {
            panic!("a driverless server answers, it does not forward");
        };
        assert_eq!(err.id, "api.admin.upload_brand_image.storage.app_error");
        assert_eq!(err.status_code, 501);
        assert_eq!(err.where_, "SaveBrandImage");
    }

    /// With a driver configured the guard passes and the re-encode is handed over — **before**
    /// the archive `MoveFile` and before the `WriteFile`, so a forwarded upload has left nothing
    /// in the backend for Go to trip over.
    #[tokio::test]
    async fn a_configured_server_forwards_the_re_encode_without_writing() {
        let app = crate::App::with_config(unreachable_store(), crate::config::Config::default());
        let err = app
            .save_brand_image()
            .await
            .expect_err("nothing here can produce Go's PNG bytes");
        assert!(
            matches!(err, PrepareError::Unreproducible(_)),
            "a configured server forwards rather than answering"
        );
    }
}
