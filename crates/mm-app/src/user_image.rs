//! Port of the profile-picture write behind `POST /api/v4/users/{user_id}/image`:
//! `SetProfileImageFromMultiPartFile` → `AdjustImage` → `SetProfileImageFromFile`
//! (app/user.go:1078-1140).
//!
//! Every accepted upload is **re-encoded** — decoded, turned upright by its EXIF orientation,
//! `FillCenter`ed to 128×128 and written as a best-compression PNG — so the stored
//! `users/<id>/profile.png` is Go's encoder's output, never the client's bytes. That output is
//! reproduced byte for byte through [`crate::image_pipeline`] for PNG and JPEG uploads. A GIF,
//! BMP, TIFF or WebP is refused as [`PrepareError::Unreproducible`] before anything is decoded or
//! written, and the handler forwards the request ([D-411]).

use mm_model::utils::AppError;
use mm_store::UserStore;

use crate::App;
use crate::image_pipeline::{self, PipelineError};
use crate::imaging_orientation::{Input, get_image_orientation};
use crate::post::PrepareError;

impl App {
    /// Port of `App.SetProfileImageFromMultiPartFile` (app/user.go:1078) and everything under it.
    ///
    /// In Go's order:
    ///
    /// 1. `checkImageLimits`: `image.DecodeConfig` — any failure, including "not an image", is the
    ///    **400** `check_image_limits`; so is a declared size past `MaxImageResolution`.
    /// 2. `AdjustImage`: the guarded decode (400 `decode`), the orientation through a **seekable**
    ///    reader with the decoder's format, `MakeImageUpright`, `FillCenter(128, 128)`, `EncodePNG`
    ///    (500 `encode`).
    /// 3. If `users/<id>/profile.png` already holds exactly these bytes, **nothing else happens** —
    ///    no write, no `LastPictureUpdate`, no event.
    /// 4. The write (500 `upload_profile`), `UpdateLastPictureUpdate` (logged on failure), and
    ///    `invalidateUserCacheAndPublish`.
    #[tracing::instrument(skip(self, data), fields(bytes = data.len()))]
    pub async fn set_profile_image(&self, user_id: &str, data: &[u8]) -> Result<(), PrepareError> {
        let error = |id: &str, status: i32| {
            PrepareError::App(AppError::boxed(
                "SetProfileImage",
                id,
                None,
                String::new(),
                status,
            ))
        };
        let wrapped = |id: &str, status: i32, err: PipelineError| {
            PrepareError::App(Box::new(
                AppError::new("SetProfileImage", id, None, String::new(), status).wrap(err),
            ))
        };
        let max_res = self.config().file_max_image_resolution;

        // `checkImageLimits` → `imaging.GetDimensions`: `image.DecodeConfig`, unwrapped.
        match goimage::format::decode_config(data) {
            Err(goimage::format::DecodeError::NotPorted(_)) => {
                return Err(PrepareError::Unreproducible(
                    "GIF, BMP, TIFF and WebP profile pictures are decoded by Go",
                ));
            }
            Err(goimage::format::DecodeError::Go(err)) => {
                return Err(wrapped(
                    "api.user.upload_profile_user.check_image_limits.app_error",
                    400,
                    PipelineError::Go(format!("failed to get image dimensions: {err}")),
                ));
            }
            Ok((config, _)) => {
                if let Err(err) = crate::imaging::check_image_resolution_limit(
                    config.width,
                    config.height,
                    max_res,
                ) {
                    return Err(wrapped(
                        "api.user.upload_profile_user.check_image_limits.app_error",
                        400,
                        PipelineError::Go(err.to_string()),
                    ));
                }
            }
        }

        let owned = data.to_vec(); // moved to the blocking pool
        let adjusted = tokio::task::spawn_blocking(move || adjust_image(&owned, max_res))
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "profile image processing panicked");
                error("api.user.upload_profile_user.encode.app_error", 500)
            })?;
        let png = match adjusted {
            Ok(png) => png,
            Err(Adjust::Decode(err)) => {
                return Err(wrapped(
                    "api.user.upload_profile_user.decode.app_error",
                    400,
                    err,
                ));
            }
            Err(Adjust::Encode(err)) => {
                return Err(wrapped(
                    "api.user.upload_profile_user.encode.app_error",
                    500,
                    err,
                ));
            }
            Err(Adjust::Unreproducible(why)) => return Err(PrepareError::Unreproducible(why)),
        };

        let path = crate::user::profile_image_path(user_id);
        if let Ok(stored) = self.read_file(&path).await {
            if stored == png {
                return Ok(());
            }
        }

        if let Err(err) = self.write_file(&png, &path).await {
            tracing::error!(error = ?err, "writing the profile image failed");
            return Err(error(
                "api.user.upload_profile_user.upload_profile.app_error",
                500,
            ));
        }

        if let Err(err) = self
            .store()
            .user()
            .update_last_picture_update(user_id)
            .await
        {
            tracing::warn!(error = %err, "Error with updating last picture update");
        }
        self.invalidate_user_cache_and_publish(user_id).await;
        // `onUserProfileChange`: the shared-channel sync service, which is never active here.
        Ok(())
    }

    /// Port of `App.invalidateUserCacheAndPublish` (app/user.go:2912): purge Go's copy of the
    /// user, re-read it, and broadcast one `user_updated` carrying the member-sanitised profile
    /// (`SanitizeProfile(options, false)`) to everyone.
    pub(crate) async fn invalidate_user_cache_and_publish(&self, user_id: &str) {
        self.invalidate_cache_for_user(user_id).await;
        let mut user = match self.get_user(user_id).await {
            Ok(user) => user,
            Err(err) => {
                tracing::error!(error = %err, user_id, "Error in getting users profile");
                return;
            }
        };
        self.sanitize_profile(&mut user, false);
        let mut message = mm_model::websocket_message::WebSocketEvent::new(
            mm_model::websocket_message::WEBSOCKET_EVENT_USER_UPDATED,
            "",
            "",
            "",
            None,
            "",
        );
        message.add(
            "user",
            serde_json::to_value(&user).unwrap_or(serde_json::Value::Null),
        );
        self.publish(message).await;
    }
}

/// How `AdjustImage` failed.
#[derive(Debug)]
enum Adjust {
    Decode(PipelineError),
    Encode(PipelineError),
    Unreproducible(&'static str),
}

/// Port of `App.AdjustImage` (app/user.go:1086) without the logging: decode, seek back, read the
/// orientation through the seekable reader with the decoder's format, upright, `FillCenter`
/// 128×128, `EncodePNG`.
fn adjust_image(data: &[u8], max_resolution: i64) -> Result<Vec<u8>, Adjust> {
    let (img, format) = match image_pipeline::decode(data, max_resolution) {
        Ok(decoded) => decoded,
        Err(PipelineError::NotPorted(_)) => {
            return Err(Adjust::Unreproducible(
                "GIF, BMP, TIFF and WebP profile pictures are decoded by Go",
            ));
        }
        Err(err) => return Err(Adjust::Decode(err)),
    };
    let outcome = get_image_orientation(Input::Seeker(data), format)
        .map_err(|crate::imaging_orientation::Unreproducible(why)| Adjust::Unreproducible(why))?;
    if let Some(err) = &outcome.err {
        tracing::warn!(error = %err, "Failed to get image orientation");
    }
    let upright = image_pipeline::make_image_upright(&img, outcome.orientation);
    let side = image_pipeline::PROFILE_WIDTH_AND_HEIGHT;
    image_pipeline::encode_png(&image_pipeline::fill_center(&upright, side, side))
        .map_err(Adjust::Encode)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `AdjustImage` over the pipeline oracle's `profile` column: the same bytes Go stores.
    #[test]
    fn adjust_image_matches_the_oracle_profile_column() {
        use base64::Engine as _;
        use sha2::Digest as _;
        let path = format!(
            "{}/../../fixtures/behaviour_imaging_pipeline.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let fx: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let mut n = 0;
        for c in fx["cases"].as_array().unwrap() {
            let Some(b64) = c["b64"].as_str() else {
                continue; // the inline cases carry their bytes; the others are tested in image_pipeline
            };
            let data = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap();
            let png = adjust_image(&data, 7680 * 4320).unwrap();
            let sha: String = sha2::Sha256::digest(&png)
                .iter()
                .map(|x| format!("{x:02x}"))
                .collect();
            assert_eq!(c["profile"]["sha256"], sha, "{}", c["name"]);
            n += 1;
        }
        assert!(n >= 10, "{n}");
    }
}
