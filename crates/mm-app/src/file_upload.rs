//! Port of `UploadFileX` and its `UploadFileTask` (app/file.go:697-1060) — the single-file
//! upload behind `POST /api/v4/files`, in both its simple-body and multipart forms.
//!
//! # What is served and what is handed to Go
//!
//! Everything: the storage and size refusals, the `FileInfo` an upload mints (id, dated path,
//! extension, mime type), `preprocessImage` (the SVG and raster header reads, the resolution
//! refusal, the EXIF axis swap, the derived paths), the write, the over-length removal,
//! `postprocessImage` (the `_thumb` and `_preview` files and the 16×16 `mini_preview`, byte for
//! byte through [`crate::image_pipeline`]) and the row — **for every format**. A WebP canvas
//! declaring alpha is recognised by its header and refused as [`PrepareError::Unreproducible`] **before
//! anything is written**, and the handler forwards the untouched request: those decoders are not
//! ported ([D-650]). A raster Go cannot decode is served: `preprocessImage` returns "as is" and
//! `postprocessImage` only logs, so the row is the plain one with no dimensions.
//!
//! The plugin hook (`runPluginsHook`) runs under the Rust plugin host ([`App::run_plugins_hook`])
//! and takes Go's early return otherwise; content extraction is [D-651].
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
use crate::image_pipeline::{self, PipelineError};
use crate::imaging::file_ext_from_mime_type;
use crate::imaging_orientation::{
    Input, ROTATED_CCW, ROTATED_CCW_MIRRORED, ROTATED_CW, ROTATED_CW_MIRRORED,
    get_image_orientation,
};
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
    /// `pluginContext(rctx)`, for `FileWillBeUploaded`.
    pub hook_ctx: &'a crate::plugin_hooks::HookContext,
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

        // `t.imageOrientation`: Go's zero value when preprocessing stopped early.
        let mut orientation = 0;
        if info.is_image() {
            orientation = self
                .preprocess_image(&mut info, &name, &prefix, input)
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

        let ran = self
            .run_plugins_hook(task.hook_ctx, &mut info, input.to_vec())
            .await
            .map_err(PrepareError::App)?;

        // Go makes the images from `a.FileReader(t.fileinfo.Path)` — storage, at the path as the
        // plugins left it — so a replacement is what the thumbnails show. With no plugin run
        // that is the bytes just written.
        if info.is_image() && !info.is_svg() {
            let stored;
            let image: &[u8] = if ran {
                stored = self.read_stored_upload(&info.path).await?;
                &stored
            } else {
                input
            };
            self.postprocess_image(&mut info, image, orientation)
                .await?;
        }

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

    /// `a.FileReader(path)` read to the end, with `FileReader`'s own error — the one `UploadFileX`
    /// returns when the file the plugins left behind cannot be opened.
    async fn read_stored_upload(&self, path: &str) -> Result<Vec<u8>, PrepareError> {
        use tokio::io::AsyncReadExt as _;
        let (mut file, _size) = self.file_reader(path).await?;
        let mut out = Vec::new();
        file.read_to_end(&mut out).await.map_err(|err| {
            tracing::error!(error = %err, path, "reading the stored upload failed");
            PrepareError::App(AppError::boxed(
                "ReadFile",
                "api.file.read_file.reading_local.app_error",
                None,
                String::new(),
                500,
            ))
        })?;
        Ok(out)
    }

    /// Port of `UploadFileTask.preprocessImage` (app/file.go:892). Returns the EXIF orientation
    /// `postprocessImage` will turn the image upright with.
    ///
    /// An SVG goes to `ParseSVG` for its dimensions and never gets a preview — see
    /// [`crate::imaging::parse_svg`]. A raster is `DecodeConfig`: a refusal there is "as is" (no
    /// dimensions, no preview, and the upload proceeds with orientation 0); otherwise the
    /// dimensions, the resolution refusal, `HasPreviewImage` and the two derived paths, then the
    /// orientation — read through the **non-seekable** reader Go hands in, which is not always the
    /// answer a seekable one gives — swapping width and height for the four orientations that
    /// turn the image on its side. A file the mime table calls `image/gif` is decoded whole, and
    /// when that works it gets no preview (an animated GIF's first frame is not one).
    fn preprocess_image(
        &self,
        info: &mut FileInfo,
        name: &str,
        prefix: &str,
        input: &[u8],
    ) -> Result<i64, Preprocess> {
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
            return Ok(0);
        }

        let config = match image_pipeline::decode_config(input) {
            Ok(config) => config,
            Err(PipelineError::NotPorted(_)) => {
                return Err(Preprocess::Unreproducible(
                    "a WebP canvas that declares alpha is decoded by Go",
                ));
            }
            // "If we fail to decode, return as is."
            Err(PipelineError::Go(_)) => return Ok(0),
        };
        info.width = config.width;
        info.height = config.height;
        if crate::imaging::check_image_resolution_limit(
            config.width,
            config.height,
            self.config().file_max_image_resolution,
        )
        .is_err()
        {
            return Err(Preprocess::TooLarge);
        }
        info.has_preview_image = true;
        // `t.Name[:strings.LastIndex(t.Name, ".")]` — an image mime type implies an extension, so
        // the dot is there.
        let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
        let ext = file_ext_from_mime_type(&info.mime_type);
        info.preview_path = format!("{prefix}{stem}_preview.{ext}");
        info.thumbnail_path = format!("{prefix}{stem}_thumb.{ext}");

        let outcome = get_image_orientation(Input::Stream(input), config.format).map_err(
            |crate::imaging_orientation::Unreproducible(why)| Preprocess::Unreproducible(why),
        )?;
        if let Some(err) = &outcome.err {
            tracing::warn!(error = %err, "Failed to get image orientation");
        } else if matches!(
            outcome.orientation,
            ROTATED_CW_MIRRORED | ROTATED_CCW | ROTATED_CCW_MIRRORED | ROTATED_CW
        ) {
            std::mem::swap(&mut info.width, &mut info.height);
        }

        if info.mime_type == "image/gif" {
            match image_pipeline::decode(input, self.config().file_max_image_resolution) {
                Ok(_) => info.has_preview_image = false,
                Err(PipelineError::NotPorted(_)) => {
                    return Err(Preprocess::Unreproducible(
                        "a WebP canvas that declares alpha is decoded by Go",
                    ));
                }
                Err(PipelineError::Go(_)) => {}
            }
        }
        Ok(outcome.orientation)
    }

    /// Port of `UploadFileTask.postprocessImage` (app/file.go:951): decode what was written, make
    /// it upright, and write the `_thumb` and `_preview` files and set `MiniPreview` — PNG when
    /// the decoder said `png`, JPEG at quality 90 otherwise, the mini preview always JPEG. A
    /// decode failure writes nothing and keeps the row as preprocessing left it; an encode or
    /// write failure is logged and skips only that file, as Go's goroutines do.
    async fn postprocess_image(
        &self,
        info: &mut FileInfo,
        written: &[u8],
        orientation: i64,
    ) -> Result<(), PrepareError> {
        let data = written.to_vec(); // moved to the blocking pool; the pixels outlive the borrow
        let max_res = self.config().file_max_image_resolution;
        let result = tokio::task::spawn_blocking(move || {
            image_pipeline::postprocess_image(&data, max_res, orientation)
        })
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "image postprocessing panicked");
            PrepareError::App(AppError::boxed(
                "UploadFileX",
                "app.file_info.save.app_error",
                None,
                String::new(),
                500,
            ))
        })?;
        let processed = match result {
            Ok(Some(processed)) => processed,
            Ok(None) => return Ok(()),
            // Refused before the write for every format that reaches here; kept as a guard.
            Err(_) => {
                return Err(PrepareError::Unreproducible(
                    "a WebP canvas that declares alpha is decoded by Go",
                ));
            }
        };
        for (bytes, path) in [
            (processed.derived.thumbnail, &info.thumbnail_path),
            (processed.derived.preview, &info.preview_path),
        ] {
            match bytes {
                Ok(bytes) => {
                    if let Err(err) = self.write_file(&bytes, path).await {
                        tracing::error!(error = ?err, path, "Unable to upload");
                    }
                }
                Err(err) => tracing::error!(error = %err, path, "Unable to encode image"),
            }
        }
        if info.mini_preview.is_none() {
            match processed.mini_preview {
                Ok(mini) => info.mini_preview = Some(mini),
                Err(err) => tracing::info!(error = %err, "Unable to generate mini preview image"),
            }
        }
        Ok(())
    }
}

/// `UploadFileForUserAndTeam`'s stand-ins for an upload that names no user or team
/// (app/file.go:625): the literal owner `nouser` and the team `noteam`.
const NO_USER: &str = "nouser";

impl App {
    /// Port of `app.App.UploadFile` (app/file.go:615) — `UploadFileForUserAndTeam` with no user
    /// and no team — the plugin API's `UploadFile`.
    ///
    /// The channel must exist unless it is empty, and any lookup failure is the 400
    /// `api.file.upload_file.incorrect_channelId.app_error`. Then
    /// [`App::do_upload_file_expect_modification`], and `HandleImages` over the bytes the
    /// **caller** passed — not a plugin's replacement — at the paths the row ended with.
    ///
    /// # Not implemented, decided before anything is written
    ///
    /// A `WebP` canvas declaring alpha (its header is read first; [`PrepareError::Unreproducible`]).
    /// A file backend this port does not drive is only discovered at the write, **after** the
    /// plugins ran, and answered the same way.
    #[tracing::instrument(skip(self, hook_ctx, data), fields(channel_id = %channel_id, filename = %filename))]
    pub async fn upload_file(
        &self,
        hook_ctx: &crate::plugin_hooks::HookContext,
        data: Vec<u8>,
        channel_id: &str,
        filename: &str,
    ) -> Result<FileInfo, PrepareError> {
        if self.get_channel(channel_id).await.is_err() && !channel_id.is_empty() {
            return Err(PrepareError::App(AppError::boxed(
                "UploadFile",
                "api.file.upload_file.incorrect_channelId.app_error",
                Some(HashMap::from([(
                    "channelId".to_owned(),
                    serde_json::Value::String(channel_id.to_owned()),
                )])),
                String::new(),
                400,
            )));
        }

        let info = self
            .do_upload_file_expect_modification(
                hook_ctx,
                chrono::Local::now(),
                FILE_TEAM_ID,
                channel_id,
                NO_USER,
                filename,
                &data,
            )
            .await?;

        if !info.preview_path.is_empty() || !info.thumbnail_path.is_empty() {
            // Go's `HandleImages` cannot fail. The one refusal the pipeline has — a format it
            // does not decode — was taken on these same bytes before anything was written, unless
            // a plugin gave a non-image derived paths; then the images are skipped, logged.
            if let Err(err) = self.handle_images(&info, data).await {
                tracing::warn!(error = %err, file_id = %info.id, "HandleImages skipped");
            }
        }
        Ok(info)
    }

    /// Port of `app.App.DoUploadFileExpectModification` (app/file.go:1063), less the modified
    /// bytes it also returns, which its one migrated caller drops.
    ///
    /// In Go's order: every name through `filepath.Base` (so an empty channel is `.` in the
    /// path), [`get_info_for_bytes`] (a failure is a 400 whatever it was), the EXIF orientation
    /// read **by mime type**, which swaps the dimensions for the four sideways orientations and
    /// only logs when it cannot be read, the id, owner and `CreateAt`, the dated path, and — for
    /// a raster image — the resolution refusal and the two derived paths. Then the plugins'
    /// `FileWillBeUploaded` ([`App::run_upload_file_hooks`]), the write (of their replacement,
    /// when one wrote any, at the path as they left it) and the row.
    ///
    /// **`ChannelId` is never set** — the channel reaches the path and nothing else — and the row
    /// has no `MiniPreview`: this path generates none, so the first `GetFileInfo` of an image
    /// uploaded here is the repair [D-891] keeps with Go. Content extraction is [D-651].
    #[allow(clippy::too_many_arguments)]
    pub async fn do_upload_file_expect_modification(
        &self,
        hook_ctx: &crate::plugin_hooks::HookContext,
        now: chrono::DateTime<chrono::Local>,
        raw_team_id: &str,
        raw_channel_id: &str,
        raw_user_id: &str,
        raw_filename: &str,
        data: &[u8],
    ) -> Result<FileInfo, PrepareError> {
        let filename = go_path::base(raw_filename);
        let team_id = go_path::base(raw_team_id);
        let channel_id = go_path::base(raw_channel_id);
        let user_id = go_path::base(raw_user_id);

        let mut info = get_info_for_bytes(&filename, data)?;

        match get_image_orientation(Input::Seeker(data), &info.mime_type) {
            Ok(outcome) => match &outcome.err {
                None if matches!(
                    outcome.orientation,
                    ROTATED_CW_MIRRORED | ROTATED_CCW | ROTATED_CCW_MIRRORED | ROTATED_CW
                ) =>
                {
                    std::mem::swap(&mut info.width, &mut info.height);
                }
                None => {}
                Some(err) => tracing::warn!(error = %err, "Failed to get image orientation"),
            },
            Err(crate::imaging_orientation::Unreproducible(why)) => {
                return Err(PrepareError::Unreproducible(why));
            }
        }

        info.id = new_id();
        info.creator_id.clone_from(&user_id);
        info.create_at = now.timestamp_millis();
        let prefix = path_prefix(&info.id, &user_id, &team_id, &channel_id, now);
        info.path = format!("{prefix}{filename}");

        if info.is_image() && !info.is_svg() {
            if crate::imaging::check_image_resolution_limit(
                info.width,
                info.height,
                self.config().file_max_image_resolution,
            )
            .is_err()
            {
                return Err(PrepareError::App(AppError::boxed(
                    "uploadFile",
                    "api.file.upload_file.large_image.app_error",
                    Some(HashMap::from([(
                        "Filename".to_owned(),
                        serde_json::Value::String(filename.clone()),
                    )])),
                    String::new(),
                    400,
                )));
            }
            // `filename[:strings.LastIndex(filename, ".")]`: an image mime type came from an
            // extension, so the dot is there.
            let stem = filename
                .rsplit_once('.')
                .map_or(filename.as_str(), |(stem, _)| stem);
            let ext = file_ext_from_mime_type(&info.mime_type);
            info.preview_path = format!("{prefix}{stem}_preview.{ext}");
            info.thumbnail_path = format!("{prefix}{stem}_thumb.{ext}");
        }

        let replaced = self
            .run_upload_file_hooks(hook_ctx, &mut info, data)
            .await
            .map_err(PrepareError::App)?;
        let bytes = replaced.as_deref().unwrap_or(data);

        self.write_file(bytes, &info.path).await?;

        match self.store().file_info().save(info).await {
            Ok(info) => Ok(info),
            Err(StoreError::Invalid { app_error, .. }) => Err(PrepareError::App(app_error)),
            Err(err) => {
                tracing::error!(error = %err, "FileInfo save failed");
                Err(PrepareError::App(AppError::boxed(
                    "DoUploadFileExpectModification",
                    "app.file_info.save.app_error",
                    None,
                    String::new(),
                    500,
                )))
            }
        }
        // `ExtractContent`: [D-651].
    }
}

/// Port of `getInfoForBytes` (app/file_info.go:18) — `DoUploadFileExpectModification`'s row
/// before it has an id, whose one refusal is a GIF.
///
/// The name as given, the size of the bytes, the lower-cased extension and the mime type it
/// maps to. For an image name, the header's dimensions when `image.DecodeConfig` reads one — and
/// a header that does not decode leaves the file an image with no dimensions and **no** preview.
/// A decoded GIF has a preview only when it has exactly one frame, and a GIF whose frames do not
/// count is the 400 `app.file_info.get.gif.app_error` (its `HasPreviewImage` set on the way out,
/// which nobody reads). Every other decoded image has a preview.
///
/// Go's `err` is declared and never assigned outside the GIF branch — every other failure is
/// shadowed — so there is no other refusal. A `WebP` canvas declaring alpha, which this port does
/// not decode, is [`PrepareError::Unreproducible`].
pub fn get_info_for_bytes(name: &str, data: &[u8]) -> Result<FileInfo, PrepareError> {
    let extension = mm_model::file_info::file_extension(name);
    let mut info = FileInfo {
        name: name.to_owned(),
        size: i64::try_from(data.len()).unwrap_or(i64::MAX),
        mime_type: crate::mime::type_by_extension(&format!(".{extension}")),
        extension,
        ..FileInfo::default()
    };
    if !info.is_image() {
        return Ok(info);
    }
    let config = match image_pipeline::decode_config(data) {
        Ok(config) => config,
        Err(PipelineError::NotPorted(_)) => {
            return Err(PrepareError::Unreproducible(
                "a WebP canvas that declares alpha is decoded by Go",
            ));
        }
        Err(PipelineError::Go(_)) => return Ok(info),
    };
    info.width = config.width;
    info.height = config.height;
    if info.mime_type == "image/gif" {
        match crate::link_image::count_gif_frames(data) {
            Ok(frames) => info.has_preview_image = frames == 1,
            Err(err) => {
                tracing::debug!(error = %err, "the GIF's frames do not count");
                return Err(PrepareError::App(AppError::boxed(
                    "getInfoForBytes",
                    "app.file_info.get.gif.app_error",
                    None,
                    String::new(),
                    400,
                )));
            }
        }
    } else {
        info.has_preview_image = true;
    }
    Ok(info)
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

    /// `imgutils.GenGIFData(1, 1, frames)`; zero frames leaves no frame and no trailer.
    fn gif(frames: usize) -> Vec<u8> {
        let mut data = vec![
            b'G', b'I', b'F', b'8', b'9', b'a', 1, 0, 1, 0, 128, 0, 0, 0, 0, 0, 1, 1, 1,
        ];
        if frames == 0 {
            return data;
        }
        for _ in 0..frames {
            data.extend_from_slice(&[0x2c, 0, 0, 0, 0, 1, 0, 1, 0, 0, 0x2, 0x2, 0x4c, 0x1, 0]);
        }
        data.push(0x3b);
        data
    }

    #[test]
    fn a_text_file_is_its_name_size_and_mime_type() {
        let info = get_info_for_bytes("Notes.TXT", b"hello").expect("an info");
        assert_eq!(
            (info.name.as_str(), info.extension.as_str(), info.size),
            ("Notes.TXT", "txt", 5)
        );
        assert_eq!(info.mime_type, "text/plain; charset=utf-8");
        assert!(!info.has_preview_image);
        let bare = get_info_for_bytes("noext", b"").expect("an info");
        assert_eq!((bare.extension.as_str(), bare.mime_type.as_str()), ("", ""));
    }

    /// A decoded image has its dimensions and a preview; one that does not decode has neither,
    /// and is still an image, with no refusal.
    #[test]
    fn an_image_is_measured_only_when_its_header_decodes() {
        const TINY_PNG: &[u8] = &[
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1,
            8, 2, 0, 0, 0, 144, 119, 83, 222, 0, 0, 0, 12, 73, 68, 65, 84, 120, 156, 99, 248, 207,
            192, 0, 0, 3, 1, 1, 0, 201, 254, 146, 239, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96,
            130,
        ];
        let png = get_info_for_bytes("p.png", TINY_PNG).expect("an info");
        assert_eq!((png.width, png.height, png.has_preview_image), (1, 1, true));
        let garbage = get_info_for_bytes("g.png", b"not a png").expect("an info");
        assert_eq!(
            (garbage.width, garbage.height, garbage.has_preview_image),
            (0, 0, false)
        );
        assert!(garbage.is_image());
    }

    /// A GIF has a preview only with exactly one frame, and one whose frames do not count is
    /// the one refusal, a 400.
    #[test]
    fn a_gif_is_previewed_only_with_one_frame() {
        assert!(
            get_info_for_bytes("s.gif", &gif(1))
                .expect("an info")
                .has_preview_image
        );
        assert!(
            !get_info_for_bytes("a.gif", &gif(2))
                .expect("an info")
                .has_preview_image
        );
        match get_info_for_bytes("t.gif", &gif(0)) {
            Err(PrepareError::App(err)) => {
                assert_eq!(
                    (err.id.as_str(), err.status_code, err.where_.as_str()),
                    ("app.file_info.get.gif.app_error", 400, "getInfoForBytes")
                );
            }
            other => panic!("a GIF with no frames: {other:?}"),
        }
        // A `.gif` whose header does not decode is not counted at all.
        let garbage = get_info_for_bytes("x.gif", b"GIF").expect("an info");
        assert!(!garbage.has_preview_image);
    }
}
