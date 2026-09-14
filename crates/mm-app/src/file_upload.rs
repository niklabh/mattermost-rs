//! Port of `UploadFileX` and its `UploadFileTask` (app/file.go:697-1060) — the single-file
//! upload behind `POST /api/v4/files`, in both its simple-body and multipart forms.
//!
//! # What is served and what is handed to Go
//!
//! Everything up to and including the write is here: the storage and size refusals, the
//! `FileInfo` an upload mints (id, dated path, extension, mime type), the SVG and PNG header
//! reads that put `width`/`height` on the wire, the resolution refusal, the write itself, the
//! over-length removal and the row. What is not here is `postprocessImage`: decoding the raster,
//! resizing it to a `_thumb` and a `_preview`, and encoding the 16×16 `mini_preview` — the pixel
//! work [D-380] and [D-411] defer. So a raster image this port can measure and Go would resize
//! is refused as [`PrepareError::Unreproducible`] **before anything is written**, and the
//! handler forwards the untouched request. A raster image Go *cannot* decode is served: Go's
//! `preprocessImage` returns "as is" and its `postprocessImage` only logs, so the row is the
//! plain one with no dimensions.
//!
//! The plugin hook (`runPluginsHook`) has no plugin host to run against and takes Go's own early
//! return; content extraction is [D-651].
//!
//! # The two size limits are not the same number
//!
//! `ContentLength > MaxFileSize` is refused before a byte is read; a body with no
//! `Content-Length` (multipart parts always, chunked bodies sometimes) is read through a
//! `LimitedReader` of `MaxFileSize + 1` and refused **after** the write when more than
//! `MaxFileSize` landed — and the file is removed again. Both are the same 413 and the same id.

use std::collections::HashMap;

use mm_model::file_info::{BOOKMARK_FILE_OWNER, FileInfo, new_info};
use mm_model::go_path;
use mm_model::utils::{AppError, new_id};
use mm_store::error::StoreError;
use mm_store::file_info_store::FileInfoStore;

use crate::App;
use crate::imaging::{ImageConfig, decode_config, file_ext_from_mime_type};
use crate::post::PrepareError;

/// `FileTeamId` (api4/file.go:26) — every REST upload is filed under this team.
pub const FILE_TEAM_ID: &str = "noteam";

/// The options `uploadFileSimple` and `uploadFileMultipart` pass to `UploadFileX`, plus the
/// bytes. `content_length` is `-1` for a multipart part, as in Go.
#[derive(Debug, Clone, Copy)]
pub struct UploadFileTask<'a> {
    pub channel_id: &'a str,
    pub name: &'a str,
    pub team_id: &'a str,
    pub user_id: &'a str,
    pub timestamp: chrono::DateTime<chrono::Local>,
    pub content_length: i64,
    pub client_id: &'a str,
    pub data: &'a [u8],
}

impl App {
    /// Port of `app.App.UploadFileX` (app/file.go:792). See the module docs for the boundary.
    ///
    /// The channel, team and file names are all passed through `filepath.Base` — the options do
    /// it for the team and the function itself for the other two — so a `channel_id` of
    /// `../x` reaches the path as `x`. Only the base is validated by the handler, before this.
    #[tracing::instrument(skip_all, fields(file_name = %task.name, channel_id = %task.channel_id, user_id = %task.user_id))]
    pub async fn upload_file_x(&self, task: &UploadFileTask<'_>) -> Result<FileInfo, PrepareError> {
        let channel_id = go_path::base(task.channel_id);
        let name = go_path::base(task.name);
        let team_id = go_path::base(task.team_id);
        let max_file_size = self.config().file_max_file_size;

        let refusal = |id: &str,
                       status: i32,
                       info: Option<&FileInfo>,
                       extra: &[(&str, serde_json::Value)]| {
            let mut params: HashMap<String, serde_json::Value> = HashMap::from([
                ("Name".to_owned(), serde_json::Value::String(name.clone())),
                (
                    "Filename".to_owned(),
                    serde_json::Value::String(name.clone()),
                ),
                (
                    "ChannelId".to_owned(),
                    serde_json::Value::String(channel_id.clone()),
                ),
                (
                    "TeamId".to_owned(),
                    serde_json::Value::String(team_id.clone()),
                ),
                (
                    "UserId".to_owned(),
                    serde_json::Value::String(task.user_id.to_owned()),
                ),
                (
                    "ContentLength".to_owned(),
                    serde_json::Value::from(task.content_length),
                ),
                (
                    "ClientId".to_owned(),
                    serde_json::Value::String(task.client_id.to_owned()),
                ),
            ]);
            if let Some(info) = info {
                params.insert("Width".to_owned(), serde_json::Value::from(info.width));
                params.insert("Height".to_owned(), serde_json::Value::from(info.height));
            }
            for (key, value) in extra {
                params.insert((*key).to_owned(), value.clone());
            }
            PrepareError::App(AppError::boxed(
                "uploadFileTask",
                id,
                Some(params),
                String::new(),
                status,
            ))
        };
        let too_large = |info: Option<&FileInfo>| {
            refusal(
                "api.file.upload_file.too_large_detailed.app_error",
                413,
                info,
                &[
                    ("Length", serde_json::Value::from(task.content_length)),
                    ("Limit", serde_json::Value::from(max_file_size)),
                ],
            )
        };

        if self.config().file_driver_name.is_empty() {
            return Err(refusal(
                "api.file.upload_file.storage.app_error",
                501,
                None,
                &[],
            ));
        }
        if task.content_length > max_file_size {
            return Err(too_large(None));
        }

        // `init`: the row as minted, before a byte is read.
        let extension = mm_model::file_info::file_extension(&name);
        let mut info = new_info(
            &name,
            crate::mime::type_by_extension(&format!(".{extension}")),
        );
        info.id = new_id();
        info.creator_id = task.user_id.to_owned();
        info.create_at = task.timestamp.timestamp_millis();
        let prefix = path_prefix(
            &info.id,
            task.user_id,
            &team_id,
            &channel_id,
            task.timestamp,
        );
        info.path = format!("{prefix}{name}");
        info.channel_id.clone_from(&channel_id);

        let limit = if task.content_length > 0 {
            task.content_length
        } else {
            max_file_size
        };
        // `io.LimitedReader{N: t.limit + 1}` — one byte more than the limit, so an over-long body
        // is *detected* by the write rather than silently cut to size.
        let input_len = usize::try_from(limit.saturating_add(1)).unwrap_or(usize::MAX);
        let input = &task.data[..task.data.len().min(input_len)];

        if info.is_image() {
            self.preprocess_image(&mut info, &name, &prefix, input)
                .map_err(|reason| match reason {
                    Preprocess::TooLarge => refusal(
                        "api.file.upload_file.large_image_detailed.app_error",
                        400,
                        Some(&info),
                        &[],
                    ),
                    Preprocess::Unreproducible(why) => PrepareError::Unreproducible(why),
                })?;
        }

        let written = self.write_file(input, &info.path).await?;
        if written > max_file_size {
            if let Err(err) = self.remove_file(&info.path).await {
                tracing::error!(error = %err, path = %info.path, "Failed to remove file");
            }
            return Err(too_large(Some(&info)));
        }
        info.size = written;

        // `runPluginsHook`: no plugin host. `postprocessImage`: an image reaching here is one
        // the decoder refuses, and Go's only action on that is a log line.

        match self.store().file_info().save(info).await {
            Ok(info) => Ok(info),
            Err(StoreError::Invalid { app_error, .. }) => Err(PrepareError::App(app_error)),
            Err(err) => {
                tracing::error!(error = %err, "FileInfo save failed");
                Err(PrepareError::App(AppError::boxed(
                    "UploadFileX",
                    "app.file_info.save.app_error",
                    None,
                    String::new(),
                    500,
                )))
            }
        }
        // `ExtractContent`: [D-651].
    }

    /// Port of `UploadFileTask.preprocessImage` (app/file.go:892), up to the point Go starts
    /// caching decoded pixels for `postprocessImage`.
    ///
    /// An SVG goes to `ParseSVG` for its dimensions and never gets a preview — see
    /// [`crate::imaging::parse_svg`]. A raster is `DecodeConfig`: a refusal there is "as is"
    /// (no dimensions, no preview, and the upload proceeds); a header this port measures gives
    /// the dimensions and the resolution refusal; and then — `HasPreviewImage`, the two derived
    /// paths, EXIF orientation, the GIF frame walk — is the part only Go finishes, so a measured
    /// image inside the limit is handed over here, before the write.
    fn preprocess_image(
        &self,
        info: &mut FileInfo,
        name: &str,
        prefix: &str,
        input: &[u8],
    ) -> Result<(), Preprocess> {
        if info.is_svg() {
            let (dims, err) = crate::imaging::parse_svg(input);
            if let Some(err) = err {
                tracing::warn!(error = %err, "Failed to parse SVG");
            }
            if dims.width > 0 && dims.height > 0 {
                info.width = dims.width;
                info.height = dims.height;
            }
            info.has_preview_image = false;
            return Ok(());
        }

        match decode_config(input) {
            // "If we fail to decode, return as is."
            ImageConfig::NoFormat => Ok(()),
            ImageConfig::Known { width, height, .. } => {
                info.width = width;
                info.height = height;
                if crate::imaging::check_image_resolution_limit(
                    width,
                    height,
                    self.config().file_max_image_resolution,
                )
                .is_err()
                {
                    return Err(Preprocess::TooLarge);
                }
                info.has_preview_image = true;
                // `t.Name[:strings.LastIndex(t.Name, ".")]` — an image mime type implies an
                // extension, so the dot is there.
                let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
                let ext = file_ext_from_mime_type(&info.mime_type);
                info.preview_path = format!("{prefix}{stem}_preview.{ext}");
                info.thumbnail_path = format!("{prefix}{stem}_thumb.{ext}");
                Err(Preprocess::Unreproducible(
                    "an image upload's preview, thumbnail and mini preview are Go's",
                ))
            }
            ImageConfig::Undecidable(reason) => Err(Preprocess::Unreproducible(reason)),
        }
    }
}

/// Why `preprocess_image` stopped.
enum Preprocess {
    TooLarge,
    Unreproducible(&'static str),
}

/// Port of `UploadFileTask.pathPrefix` (app/file.go:1028): a bookmark upload lives under
/// `bookmark/teams/…/channels/…/<id>/` with no user or date; everything else under the
/// `20060102` of the request's timestamp, in the server's local zone.
pub fn path_prefix(
    file_id: &str,
    user_id: &str,
    team_id: &str,
    channel_id: &str,
    timestamp: chrono::DateTime<chrono::Local>,
) -> String {
    if user_id == BOOKMARK_FILE_OWNER {
        return format!("{BOOKMARK_FILE_OWNER}/teams/{team_id}/channels/{channel_id}/{file_id}/");
    }
    format!(
        "{}/teams/{team_id}/channels/{channel_id}/users/{user_id}/{file_id}/",
        timestamp.format("%Y%m%d")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prefix_is_dated_for_a_user_and_undated_for_a_bookmark() {
        let at = chrono::Local
            .with_ymd_and_hms(2026, 9, 14, 10, 0, 0)
            .single()
            .unwrap();
        assert_eq!(
            path_prefix("fileid", "userid", "noteam", "chan", at),
            "20260914/teams/noteam/channels/chan/users/userid/fileid/"
        );
        assert_eq!(
            path_prefix("fileid", "bookmark", "noteam", "chan", at),
            "bookmark/teams/noteam/channels/chan/fileid/"
        );
    }

    use chrono::TimeZone;
}
