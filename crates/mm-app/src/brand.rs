//! Port of `server/channels/app/brand.go`: the read, the delete and the upload of the one brand
//! image, `brand/image.png`. The upload re-encodes whatever it is given as a PNG, byte for byte
//! Go's ([`crate::image_pipeline`]), and archives the previous image under a timestamped name.

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

    /// Port of `app.App.SaveBrandImage` (app/brand.go:22).
    ///
    /// In Go's order: the empty-driver **501** (which `uploadBrandImage` reaches only after its
    /// `edit_brand` check, so an unprivileged caller on a driverless server gets the 403);
    /// `checkImageLimits` (400 `check_image_limits`); the guarded decode (400 `decode`);
    /// `EncodePNG` (500 `encode`) — so the stored bytes are Go's encoder's for every accepted
    /// upload, a PNG included; then the existing image is **archived** by `MoveFile` to
    /// `brand/<2006-01-02T15:04:05>.png` in the server's local time (both failures only logged);
    /// and the write (500 `save_image`).
    ///
    /// A WebP canvas declaring alpha is [`PrepareError::Unreproducible`] — handed to Go before the
    /// archive and the write, so a forwarded upload has not touched the backend ([D-411]).
    pub async fn save_brand_image(&self, data: &[u8]) -> Result<(), PrepareError> {
        use crate::image_pipeline::{self, PipelineError};

        let error = |id: &str, status: i32, err: Option<PipelineError>| {
            let app_error = AppError::new("SaveBrandImage", id, None, String::new(), status);
            PrepareError::App(Box::new(match err {
                Some(err) => app_error.wrap(err),
                None => app_error,
            }))
        };
        if self.config().file_driver_name.is_empty() {
            return Err(error(
                "api.admin.upload_brand_image.storage.app_error",
                501,
                None,
            ));
        }

        let max_res = self.config().file_max_image_resolution;
        match goimage::format::decode_config(data) {
            Err(goimage::format::DecodeError::NotPorted(_)) => {
                return Err(PrepareError::Unreproducible(
                    "a WebP canvas that declares alpha is decoded by Go",
                ));
            }
            Err(goimage::format::DecodeError::Go(err)) => {
                return Err(error(
                    "brand.save_brand_image.check_image_limits.app_error",
                    400,
                    Some(PipelineError::Go(format!(
                        "failed to get image dimensions: {err}"
                    ))),
                ));
            }
            Ok((config, _)) => {
                if let Err(err) = crate::imaging::check_image_resolution_limit(
                    config.width,
                    config.height,
                    max_res,
                ) {
                    return Err(error(
                        "brand.save_brand_image.check_image_limits.app_error",
                        400,
                        Some(PipelineError::Go(err.to_string())),
                    ));
                }
            }
        }

        let owned = data.to_vec(); // moved to the blocking pool
        let encoded = tokio::task::spawn_blocking(move || {
            let (img, _) = image_pipeline::decode(&owned, max_res)?;
            Ok::<_, PipelineError>(image_pipeline::encode_png(&img))
        })
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "brand image processing panicked");
            error("brand.save_brand_image.encode.app_error", 500, None)
        })?;
        let png = match encoded {
            Ok(Ok(png)) => png,
            Ok(Err(err)) => {
                return Err(error(
                    "brand.save_brand_image.encode.app_error",
                    500,
                    Some(err),
                ));
            }
            Err(PipelineError::NotPorted(_)) => {
                return Err(PrepareError::Unreproducible(
                    "a WebP canvas that declares alpha is decoded by Go",
                ));
            }
            Err(err) => {
                return Err(error(
                    "brand.save_brand_image.decode.app_error",
                    400,
                    Some(err),
                ));
            }
        };

        let old_path = brand_image_path();
        let new_path = format!(
            "{BRAND_FILE_PATH}{}.png",
            chrono::Local::now().format("%Y-%m-%dT%H:%M:%S")
        );
        match self.file_exists(&old_path).await {
            Ok(true) => {
                if let Err(err) = self.move_file(&old_path, &new_path).await {
                    tracing::warn!(error = ?err, old_path, new_path, "Failed to backup old brand image");
                }
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(error = ?err, path = old_path, "Failed to check if brand image exists before backup");
            }
        }

        if let Err(err) = self.write_file(&png, &old_path).await {
            tracing::error!(error = ?err, "writing the brand image failed");
            return Err(error(
                "brand.save_brand_image.save_image.app_error",
                500,
                None,
            ));
        }
        Ok(())
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
            .save_brand_image(b"GIF89a\x01\x00\x01\x00")
            .await
            .expect_err("no file driver, no upload")
        else {
            panic!("a driverless server answers, it does not forward");
        };
        assert_eq!(err.id, "api.admin.upload_brand_image.storage.app_error");
        assert_eq!(err.status_code, 501);
        assert_eq!(err.where_, "SaveBrandImage");
    }

    /// A lossy WebP carrying an `ALPH` chunk, out of the imaging oracle's own corpus: Go decodes
    /// it into an `*image.NYCbCrA`, which `goimage::image::Image` does not model.
    fn alpha_webp() -> Vec<u8> {
        use base64::Engine as _;
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/behaviour_imaging_webp.json"
        ))
        .unwrap();
        let fx: serde_json::Value = serde_json::from_str(&text).unwrap();
        let c = fx["decode"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "yellow_rose.lossy-with-alpha.webp")
            .unwrap();
        base64::engine::general_purpose::STANDARD
            .decode(c["b64"].as_str().unwrap())
            .unwrap()
    }

    /// With a driver configured a format this port does not decode is handed over — **before**
    /// the archive `MoveFile` and before the `WriteFile`, so a forwarded upload has left nothing
    /// in the backend for Go to trip over. (The store is unreachable: a write would fail loudly.)
    ///
    /// The bytes are a WebP canvas declaring alpha — the one answer this port does not have —
    /// rather than the GIF this test used to send: every other format is decoded here now, so a
    /// GIF would reach the store and this test would be measuring the store.
    #[tokio::test]
    async fn an_unported_format_forwards_without_writing() {
        let app = crate::App::with_config(unreachable_store(), crate::config::Config::default());
        let err = app
            .save_brand_image(&alpha_webp())
            .await
            .expect_err("a lossy WebP with an alpha chunk is Go's to decode");
        assert!(
            matches!(err, PrepareError::Unreproducible(_)),
            "a configured server forwards rather than answering"
        );
    }

    /// Bytes no decoder claims are `checkImageLimits`' 400, answered here.
    #[tokio::test]
    async fn a_non_image_is_the_check_image_limits_400() {
        let app = crate::App::with_config(unreachable_store(), crate::config::Config::default());
        let Err(PrepareError::App(err)) = app.save_brand_image(b"not an image").await else {
            panic!("answered here");
        };
        assert_eq!(
            err.id,
            "brand.save_brand_image.check_image_limits.app_error"
        );
        assert_eq!(err.status_code, 400);
    }
}
