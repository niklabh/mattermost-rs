//! Port of `server/channels/app/upload.go` — the resumable upload: `CreateUploadSession`,
//! `GetUploadSession`, `GetUploadSessionsForUser` and `UploadData`.
//!
//! # Where the port stops, and why it stops *before* writing
//!
//! `UploadData`'s completion step for an image runs `HandleImages` (app/file.go:1161): it decodes
//! the file, resizes it twice and encodes a `_preview` and a `_thumb` beside it. That is the
//! pixel work [D-380] and [D-411] defer, so an upload whose last chunk would reach it is handed
//! to Go as [`PrepareError::Unreproducible`] — and the decision is taken from the file's first
//! bytes **before the chunk is written**, because a request forwarded after the write would find
//! `FileOffset == FileSize` on Go's side and be refused as over-long
//! (`api.upload.upload_data.invalid_content_length`). Nothing else in the flow needs a decoder:
//! an image Go cannot decode is a 500 here and there, and one past `MaxImageResolution` is a 400
//! here and there, both from the header alone.
//!
//! The `FileWillBeUploaded` plugin hook (`runPluginsHook`) is not applicable: there is no plugin
//! host, so the hook list is empty and Go's own early return (`hookHasRunCh` closed unread) is the
//! path taken. Content extraction (`ExtractContentFromFileInfo`, a goroutine) is [D-651].

use std::collections::HashMap;

use mm_model::file_info::{FileInfo, sanitize_filename};
use mm_model::go_path;
use mm_model::upload_session::{INCOMPLETE_UPLOAD_SUFFIX, UploadSession, UploadType};
use mm_model::utils::{AppError, AppResult};
use mm_store::UploadSessionStore;
use mm_store::error::StoreError;
use mm_store::file_info_store::FileInfoStore;

use crate::App;
use crate::channel::RestrictedDm;
use crate::imaging::{ImageConfig, decode_config};
use crate::post::PrepareError;

/// Port of `minFirstPartSize` (app/upload.go:23) — 5 MiB. A first chunk shorter than this that
/// does not also finish the file is removed and refused; the resume path has no such floor.
pub const MIN_FIRST_PART_SIZE: i64 = 5 * 1024 * 1024;

/// How much of the stored file the completion step reads to classify an image: enough for every
/// magic prefix in [`crate::imaging`] and for a PNG's `IHDR`.
const IMAGE_HEAD_BYTES: usize = 64;

impl App {
    /// Port of `app.App.CreateUploadSession` (app/upload.go:130).
    ///
    /// `FileOffset` is reset and `CreateAt` stamped from one `time.Now()` that also dates the
    /// path — `20060102`, in the **server's local zone**, so a session created at 00:30 in
    /// Kolkata lands under the day before UTC's. The attachment path always says `teams/noteam`;
    /// the import path is `filepath.Clean(ImportSettings.Directory)/<id>_<base(filename)>`.
    ///
    /// The three channel refusals are all **400** with their own ids — a missing channel is not a
    /// 404 here — and every store failure is the one 500 `app.upload.create.save.app_error`,
    /// including a validation failure the handler's own checks let through.
    #[tracing::instrument(skip_all, fields(upload_id = %us.id, upload_type = %us.type_.as_str()))]
    pub async fn create_upload_session(&self, mut us: UploadSession) -> AppResult<UploadSession> {
        us.file_offset = 0;
        let now = chrono::Local::now();
        us.create_at = now.timestamp_millis();
        if us.type_.as_str() == UploadType::ATTACHMENT {
            us.path = format!(
                "{}/teams/noteam/channels/{}/users/{}/{}/{}",
                now.format("%Y%m%d"),
                us.channel_id,
                us.user_id,
                us.id,
                go_path::base(&us.filename)
            );
        } else if us.type_.as_str() == UploadType::IMPORT {
            us.path = format!(
                "{}/{}_{}",
                go_path::clean(&self.config().import_directory),
                us.id,
                go_path::base(&us.filename)
            );
        }
        us.is_valid()?;

        if us.type_.as_str() == UploadType::ATTACHMENT {
            let channel_param = || {
                Some(HashMap::from([(
                    "channelId".to_owned(),
                    serde_json::Value::String(us.channel_id.clone()),
                )]))
            };
            let channel = match self.get_channel(&us.channel_id).await {
                Ok(channel) => channel,
                Err(_) => {
                    return Err(AppError::boxed(
                        "CreateUploadSession",
                        "app.upload.create.incorrect_channel_id.app_error",
                        channel_param(),
                        String::new(),
                        400,
                    ));
                }
            };
            if channel.delete_at != 0 {
                return Err(AppError::boxed(
                    "CreateUploadSession",
                    "app.upload.create.cannot_upload_to_deleted_channel.app_error",
                    channel_param(),
                    String::new(),
                    400,
                ));
            }
            if self.check_if_channel_is_restricted_dm(&channel).await? == RestrictedDm::Yes {
                return Err(AppError::boxed(
                    "CreateUploadSession",
                    "app.upload.create.cannot_upload_to_restricted_dm.error",
                    None,
                    String::new(),
                    400,
                ));
            }
        }

        self.store().upload_session().save(us).await.map_err(|err| {
            tracing::error!(error = %err, "upload session save failed");
            AppError::boxed(
                "CreateUploadSession",
                "app.upload.create.save.app_error",
                None,
                String::new(),
                500,
            )
        })
    }

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

    /// Port of `app.App.UploadData` (app/upload.go:198), with the chunk already in memory.
    ///
    /// `Ok(None)` is an incomplete upload — the handler's 204 — and `Ok(Some(info))` the row the
    /// completing chunk created. In order:
    ///
    /// 1. **The session lock.** Go keeps a process-wide map and refuses a second concurrent
    ///    chunk for one session with 400 `app.upload.upload_data.concurrent.app_error`; so does
    ///    this, per process — a chunk sent to each server at once is refused by neither.
    /// 2. **The offset re-check** against a fresh row, the same id with `FileOffset mismatch`
    ///    as detail.
    /// 3. The chunk is cut to `FileSize - FileOffset` (Go's `LimitedReader`) — an over-long
    ///    body with no `Content-Length` is truncated, not refused.
    /// 4. **The forward decision** — see the module docs.
    /// 5. A first chunk is a `WriteFile`; under [`MIN_FIRST_PART_SIZE`] and not the whole file it
    ///    is removed and refused. A later chunk is an `AppendFile`. `FileOffset` is advanced and
    ///    the row updated **whenever bytes were written**, before any error is returned.
    /// 6. On completion the file is read back, `genFileInfoFromReader` names it (a name that
    ///    sanitises to nothing is the 400 `invalid_filename`; an image the decoder refuses is the
    ///    **500** `gen_info`), an image past `MaxImageResolution` is the 400 `large_image`, an
    ///    import is moved off its `.tmp` name, the `FileInfo` is saved, and the session deleted —
    ///    a failed delete is logged and the upload still succeeds.
    ///
    /// Note what the completion does **not** set: `HasPreviewImage` stays false and
    /// `MiniPreview` nil, unlike `UploadFileX` — a resumable upload's `FileInfo` says
    /// `"mini_preview":null` and omits `has_preview_image` even for an image.
    #[tracing::instrument(skip_all, fields(upload_id = %us.id, file_offset = us.file_offset, bytes = data.len()))]
    pub async fn upload_data(
        &self,
        mut us: UploadSession,
        data: &[u8],
    ) -> Result<Option<FileInfo>, PrepareError> {
        let concurrent = |detail: &str| {
            PrepareError::App(AppError::boxed(
                "UploadData",
                "app.upload.upload_data.concurrent.app_error",
                None,
                detail.to_owned(),
                400,
            ))
        };
        let Some(_lock) = UploadLock::acquire(self, &us.id) else {
            return Err(concurrent(""));
        };

        let stored = self.get_upload_session(&us.id).await?;
        if us.file_offset != stored.file_offset {
            return Err(concurrent("FileOffset mismatch"));
        }

        let is_import = us.type_.as_str() == UploadType::IMPORT;
        let upload_path = if is_import {
            format!("{}{INCOMPLETE_UPLOAD_SUFFIX}", us.path)
        } else {
            us.path.clone()
        };

        let remaining = usize::try_from(us.file_size - us.file_offset).unwrap_or(0);
        let chunk = &data[..data.len().min(remaining)];
        let completes =
            us.file_offset + i64::try_from(chunk.len()).unwrap_or(i64::MAX) == us.file_size;

        if completes {
            self.refuse_if_completion_needs_derived_images(&us, &upload_path, chunk)
                .await?;
        }

        let mut written: i64 = 0;
        let mut write_error: Option<PrepareError> = None;
        if us.file_offset == 0 {
            // A new upload. `WriteFile` here is all-or-nothing, so Go's `err != nil && written
            // == 0` is simply the error.
            written = self.write_file(chunk, &upload_path).await?;
            if written < MIN_FIRST_PART_SIZE && written != us.file_size {
                if let Err(err) = self.remove_file(&upload_path).await {
                    tracing::warn!(
                        error = %err,
                        upload_path,
                        upload_id = %us.id,
                        filename = %us.filename,
                        chunk_size = written,
                        min_size = MIN_FIRST_PART_SIZE,
                        "Failed to remove initial upload chunk that was too small"
                    );
                }
                return Err(PrepareError::App(AppError::boxed(
                    "UploadData",
                    "app.upload.upload_data.first_part_too_small.app_error",
                    Some(HashMap::from([(
                        "Size".to_owned(),
                        serde_json::Value::from(MIN_FIRST_PART_SIZE),
                    )])),
                    String::new(),
                    400,
                )));
            }
        } else if us.file_offset < us.file_size {
            // A resume.
            match self.append_file(chunk, &upload_path).await {
                Ok(count) => written = count,
                Err(err) => write_error = Some(err),
            }
        }

        if written > 0 {
            us.file_offset += written;
            if let Err(err) = self.store().upload_session().update(&us).await {
                tracing::error!(error = %err, "upload session update failed");
                return Err(PrepareError::App(AppError::boxed(
                    "UploadData",
                    "app.upload.upload_data.update.app_error",
                    None,
                    String::new(),
                    500,
                )));
            }
        }
        if let Some(err) = write_error {
            return Err(err);
        }

        // The upload is incomplete.
        if us.file_offset != us.file_size {
            return Ok(None);
        }

        // The upload is done: create the FileInfo.
        let file = self.read_file(&upload_path).await.map_err(|err| {
            tracing::error!(error = %err, upload_path, "reading the completed upload failed");
            PrepareError::App(AppError::boxed(
                "UploadData",
                "app.upload.upload_data.read_file.app_error",
                None,
                String::new(),
                500,
            ))
        })?;

        let mut info = match gen_file_info_from_reader(&us.filename, &file, us.file_size) {
            Ok(info) => info,
            Err(GenFileInfoError::App(err)) => return Err(PrepareError::App(err)),
            Err(GenFileInfoError::Decode(reason)) => {
                tracing::debug!(
                    reason,
                    "the completed upload's image header does not decode"
                );
                return Err(PrepareError::App(AppError::boxed(
                    "UploadData",
                    "app.upload.upload_data.gen_info.app_error",
                    None,
                    String::new(),
                    500,
                )));
            }
            Err(GenFileInfoError::Unreproducible(reason)) => {
                // Refused above before the write; reaching here means the head read and the
                // full file disagree, which a concurrent writer could arrange.
                return Err(PrepareError::Unreproducible(reason));
            }
        };

        info.creator_id.clone_from(&us.user_id);
        info.channel_id.clone_from(&us.channel_id);
        info.path.clone_from(&us.path);
        info.remote_id = Some(us.remote_id.clone());
        if !us.req_file_id.is_empty() {
            info.id.clone_from(&us.req_file_id);
        }

        // `runPluginsHook`: no plugin host, so the hook has not run and Go returns early.

        // Image post-processing.
        if info.is_image() && !info.is_svg() {
            if crate::imaging::check_image_resolution_limit(
                info.width,
                info.height,
                self.config().file_max_image_resolution,
            )
            .is_err()
            {
                return Err(PrepareError::App(AppError::boxed(
                    "uploadData",
                    "app.upload.upload_data.large_image.app_error",
                    Some(HashMap::from([
                        (
                            "Filename".to_owned(),
                            serde_json::Value::String(us.filename.clone()),
                        ),
                        ("Width".to_owned(), serde_json::Value::from(info.width)),
                        ("Height".to_owned(), serde_json::Value::from(info.height)),
                    ])),
                    String::new(),
                    400,
                )));
            }
            // Past the limit check Go derives `_preview` and `_thumb`; that is the case the
            // pre-write decision handed to Go.
            return Err(PrepareError::Unreproducible(
                "an image upload's preview and thumbnail are Go's",
            ));
        }

        if is_import {
            self.move_file(&upload_path, &us.path)
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, "moving the completed import failed");
                    PrepareError::App(AppError::boxed(
                        "UploadData",
                        "app.upload.upload_data.move_file.app_error",
                        None,
                        String::new(),
                        500,
                    ))
                })?;
        }

        let info = match self.store().file_info().save(info).await {
            Ok(info) => info,
            Err(StoreError::Invalid { app_error, .. }) => return Err(PrepareError::App(app_error)),
            Err(err) => {
                tracing::error!(error = %err, "FileInfo save failed");
                return Err(PrepareError::App(AppError::boxed(
                    "uploadData",
                    "app.upload.upload_data.save.app_error",
                    None,
                    String::new(),
                    500,
                )));
            }
        };

        // `ExtractContent`: [D-651].

        if let Err(err) = self.store().upload_session().delete(&us.id).await {
            tracing::warn!(error = %err, "Failed to delete UploadSession");
        }

        Ok(Some(info))
    }

    /// The pre-write half of the completion step — see the module docs.
    ///
    /// Only a name whose mime type says image matters. A first chunk carries its own head; a
    /// later one reads the head of what is already on disk (chunks are at least 5 MiB, so the
    /// header is there). Three verdicts: no magic matched is Go's 500 and is served; a PNG this
    /// port measures that is past the limit is Go's 400 and is served; everything else Go would
    /// resize, and is forwarded.
    async fn refuse_if_completion_needs_derived_images(
        &self,
        us: &UploadSession,
        upload_path: &str,
        chunk: &[u8],
    ) -> Result<(), PrepareError> {
        let name = sanitize_filename(&us.filename);
        let mime_type = crate::mime::type_by_extension(&format!(
            ".{}",
            mm_model::file_info::file_extension(&name)
        ));
        // `mime_type` already carries the dot-stripped extension's answer; an empty extension
        // asks for `"."`, which no table maps.
        if !mime_type.starts_with("image") {
            return Ok(());
        }

        let head: Vec<u8> = if us.file_offset == 0 {
            chunk[..chunk.len().min(IMAGE_HEAD_BYTES)].to_vec()
        } else {
            self.read_file_head(upload_path, IMAGE_HEAD_BYTES).await?
        };

        match decode_config(&head) {
            ImageConfig::NoFormat => Ok(()),
            ImageConfig::Known { width, height, .. }
                if crate::imaging::check_image_resolution_limit(
                    width,
                    height,
                    self.config().file_max_image_resolution,
                )
                .is_err() =>
            {
                Ok(())
            }
            ImageConfig::Known { .. } => Err(PrepareError::Unreproducible(
                "an image upload's preview and thumbnail are Go's",
            )),
            ImageConfig::Undecidable(reason) => Err(PrepareError::Unreproducible(reason)),
        }
    }

    /// The first `n` bytes of a stored file, or fewer if it is shorter.
    async fn read_file_head(&self, path: &str, n: usize) -> Result<Vec<u8>, PrepareError> {
        use tokio::io::AsyncReadExt;

        let (mut file, _) = self.file_reader(path).await?;
        let mut head = vec![0u8; n];
        let mut filled = 0;
        while filled < n {
            let count = file.read(&mut head[filled..]).await.map_err(|err| {
                tracing::error!(error = %err, path, "reading the upload's head failed");
                PrepareError::App(AppError::boxed(
                    "UploadData",
                    "app.upload.upload_data.read_file.app_error",
                    None,
                    String::new(),
                    500,
                ))
            })?;
            if count == 0 {
                break;
            }
            filled += count;
        }
        head.truncate(filled);
        Ok(head)
    }

    /// Port of `fileutils.CheckDirectoryConflict` (channels/utils/fileutils/fileutils.go:125):
    /// both directories made absolute (against this process's working directory, which is where
    /// a relative `./import` points for each server) and symlink-resolved when they exist, then
    /// either being a prefix of the other — with a trailing separator on both, so `plugins` and
    /// `plugins2` do not conflict.
    ///
    /// Only `filepath.Abs` can fail there (an unreadable working directory), and that failure
    /// is `createUpload`'s 500 `api.upload.create.check_directory.app_error`.
    pub fn check_directory_conflict(dir1: &str, dir2: &str) -> Result<bool, std::io::Error> {
        fn absolute(dir: &str) -> Result<String, std::io::Error> {
            let abs = if dir.starts_with('/') {
                go_path::clean(dir)
            } else {
                let cwd = std::env::current_dir()?;
                go_path::join(&[&cwd.to_string_lossy(), dir])
            };
            match std::fs::canonicalize(&abs) {
                Ok(resolved) => Ok(resolved.to_string_lossy().into_owned()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(abs),
                Err(err) => Err(err),
            }
        }
        let mut abs1 = absolute(dir1)?;
        let mut abs2 = absolute(dir2)?;
        if !abs1.ends_with('/') {
            abs1.push('/');
        }
        if !abs2.ends_with('/') {
            abs2.push('/');
        }
        Ok(abs1.starts_with(&abs2) || abs2.starts_with(&abs1))
    }
}

/// Go's `uploadLockMap` entry: taken for the length of one `UploadData`, released on every exit.
struct UploadLock<'a> {
    app: &'a App,
    id: String,
}

impl<'a> UploadLock<'a> {
    fn acquire(app: &'a App, id: &str) -> Option<Self> {
        let mut locks = app
            .upload_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !locks.insert(id.to_owned()) {
            return None;
        }
        Some(Self {
            app,
            id: id.to_owned(),
        })
    }
}

impl Drop for UploadLock<'_> {
    fn drop(&mut self) {
        self.app
            .upload_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
    }
}

/// How `genFileInfoFromReader` can fail.
#[derive(Debug)]
pub enum GenFileInfoError {
    /// The name sanitised to nothing — Go's own 400.
    App(Box<AppError>),
    /// The name says image and `DecodeConfig` refused the bytes — Go returns the decoder's
    /// error, which the caller turns into a 500.
    Decode(&'static str),
    /// The name says image and this port does not measure the format.
    Unreproducible(&'static str),
}

/// Port of `app.App.genFileInfoFromReader` (app/upload.go:25): sanitise the name, derive the
/// extension and mime type as `NewInfo` does, and for an image read the dimensions.
///
/// **Every mime type that starts with `image` goes to the decoder**, `image/svg+xml` included —
/// so an SVG completing through the resumable API is a decoder failure and a 500. That is Go's
/// behaviour, not a port bug; `UploadFileX` treats SVG separately and this function does not.
pub fn gen_file_info_from_reader(
    name: &str,
    file: &[u8],
    size: i64,
) -> Result<FileInfo, GenFileInfoError> {
    let name = sanitize_filename(name);
    if name.is_empty() {
        return Err(GenFileInfoError::App(AppError::boxed(
            "genFileInfoFromReader",
            "app.upload.gen_file_info.invalid_filename.app_error",
            None,
            String::new(),
            400,
        )));
    }

    let extension = mm_model::file_info::file_extension(&name);
    let mime_type = crate::mime::type_by_extension(&format!(".{extension}"));
    let mut info = FileInfo {
        name,
        mime_type,
        size,
        extension,
        ..FileInfo::default()
    };
    // `ext[1:]` — the client expects the extension without its period. `file_extension` has
    // already stripped it; an empty extension is empty either way.

    if info.is_image() {
        match decode_config(file) {
            ImageConfig::NoFormat => return Err(GenFileInfoError::Decode("image: unknown format")),
            ImageConfig::Known { width, height, .. } => {
                info.width = width;
                info.height = height;
            }
            ImageConfig::Undecidable(reason) => {
                return Err(GenFileInfoError::Unreproducible(reason));
            }
        }
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_that_sanitises_to_nothing_is_the_400() {
        for name in ["", ".", "..", "/", "\u{1}\u{2}"] {
            match gen_file_info_from_reader(name, b"", 3) {
                Err(GenFileInfoError::App(err)) => {
                    assert_eq!(
                        err.id,
                        "app.upload.gen_file_info.invalid_filename.app_error"
                    );
                    assert_eq!(err.status_code, 400);
                }
                other => panic!("{name:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn a_text_file_carries_the_builtin_mime_and_the_bare_extension() {
        let info = gen_file_info_from_reader("dir/notes.TXT", b"hello", 5).unwrap();
        assert_eq!(info.name, "notes.TXT");
        assert_eq!(info.extension, "txt");
        assert_eq!(info.mime_type, "text/plain; charset=utf-8");
        assert_eq!(info.size, 5);
        assert_eq!((info.width, info.height), (0, 0));
    }

    #[test]
    fn an_image_name_over_non_image_bytes_is_the_decoder_error() {
        assert!(matches!(
            gen_file_info_from_reader("a.png", b"not a png", 9),
            Err(GenFileInfoError::Decode(_))
        ));
        // SVG is `image/svg+xml`, so it goes to the raster decoder and fails there too.
        assert!(matches!(
            gen_file_info_from_reader("a.svg", b"<svg/>", 6),
            Err(GenFileInfoError::Decode(_))
        ));
    }

    #[test]
    fn a_measured_png_carries_its_dimensions() {
        // 1x1, the same bytes the parity suites upload.
        let png: &[u8] = &[
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1,
            8, 2, 0, 0, 0, 144, 119, 83, 222, 0, 0, 0, 12, 73, 68, 65, 84, 120, 156, 99, 248, 207,
            192, 0, 0, 3, 1, 1, 0, 201, 254, 146, 239, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96,
            130,
        ];
        let info = gen_file_info_from_reader("p.png", png, png.len() as i64).unwrap();
        assert_eq!((info.width, info.height), (1, 1));
        assert_eq!(info.mime_type, "image/png");
    }

    #[test]
    fn directory_conflict_is_prefix_either_way_with_a_separator() {
        assert!(App::check_directory_conflict("/tmp/a", "/tmp/a/b").unwrap());
        assert!(App::check_directory_conflict("/tmp/a/b", "/tmp/a").unwrap());
        assert!(!App::check_directory_conflict("/tmp/a", "/tmp/ab").unwrap());
        assert!(!App::check_directory_conflict("./import", "./plugins").unwrap());
        assert!(App::check_directory_conflict("./import", "./import/x").unwrap());
        assert!(App::check_directory_conflict("/tmp/../tmp/a", "/tmp/a").unwrap());
    }
}
