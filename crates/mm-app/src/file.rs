//! Port of the read side of `server/channels/app/file.go` — `GetFileInfo`, the mini-preview guard
//! every file read shares, and the thin wrappers that put an `AppError` on a
//! [`crate::filestore`] failure.
//!
//! # Two layers, and the lower one is where the surprises live
//!
//! Everything below [`App::read_file`] is a one-line delegation whose only job is to name an
//! error id. What they wrap — path joining, the ENOENT-vs-ENOTDIR split, the listing prefix —
//! is in [`crate::filestore`], which is where the behavioural oracle points.
//!
//! One thing to know before reading them: Go raises these errors from **free functions shared by
//! both backends**, so the `where` never says whether the file or the export backend failed, and
//! `listDirectory`'s says `ListExportDirectory` even when it was the file one. See
//! [`backend_errors`].

use mm_model::file_info::FileInfo;
use mm_model::utils::{AppError, AppResult};
use mm_store::file_info_store::FileInfoStore;

use crate::App;
use crate::post::PrepareError;

impl App {
    /// Port of `app.App.GetFileInfo` (app/file.go:1310) and the `Server.getFileInfo`
    /// (app/file.go:1295) it delegates to.
    ///
    /// # Three stages, and only the first one survives the port
    ///
    /// 1. **The store read.** Both its branches carry the *same* error id,
    ///    `app.file_info.get.app_error`, and differ only in status: 404 for a miss, 500 for a
    ///    query failure. A client that branches on the id cannot tell them apart, which is why
    ///    the status is asserted rather than the id alone.
    /// 2. **`isInaccessibleFile`** returns `app.file.cloud.get.app_error` (403) for a file past
    ///    a cloud plan's file limit. `GetLastAccessibleFileTime` reads a system value that only
    ///    a licence carrying a `Files` limit ever writes, so on this deployment it is `0` and
    ///    the branch cannot fire. Not reproduced — the same treatment, and for the same reason,
    ///    as `First-Inaccessible-Post-Time` in `mm_api::posts`.
    /// 3. **`generateMiniPreview`** is a *write*: for an image with no stored preview it reads
    ///    the original out of the file backend, encodes a thumbnail, returns it **and upserts
    ///    it into the row**. This port has no file backend, so the request is refused with
    ///    [`PrepareError::Unreproducible`] and the handler forwards it. See
    ///    [`App::mini_preview_would_be_generated`] for how narrow that is.
    #[tracing::instrument(skip(self), fields(file_id = %file_id))]
    pub async fn get_file_info(&self, file_id: &str) -> Result<FileInfo, PrepareError> {
        let info = self
            .store()
            .file_info()
            .get(file_id)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    AppError::boxed(
                        "GetFileInfo",
                        "app.file_info.get.app_error",
                        None,
                        String::new(),
                        404,
                    )
                } else {
                    tracing::error!(error = %err, "file info lookup failed");
                    AppError::boxed(
                        "GetFileInfo",
                        "app.file_info.get.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })
            .map_err(PrepareError::App)?;

        if Self::mini_preview_would_be_generated(&info) {
            return Err(PrepareError::Unreproducible(
                "generateMiniPreview reads the file backend and writes the row back",
            ));
        }

        Ok(info)
    }

    /// The row read `getFile` makes, which is **not** [`App::get_file_info`].
    ///
    /// `getFile` reaches past the app layer into `Store().FileInfo().GetByIds([]{id}, true, true,
    /// false)` (api4/file.go:531), and the three consequences are worth naming because they are
    /// the reason this exists as a second function:
    ///
    /// 1. `includeDeleted` is **true**, so a soft-deleted row comes back. The handler decides
    ///    what to do with it.
    /// 2. The mini-preview repair — a write — is skipped, so this read never has to be forwarded.
    /// 3. The error id is `api.file.get_file_info.app_error`, not `app.file_info.get.app_error`;
    ///    two ids for the same miss on two routes one segment apart.
    ///
    /// `GetByIds` also drops `FileInfo.Archived` on the floor (see `mm_store::file_info_store`),
    /// which is invisible here: `getFile` serves bytes and never marshals the row.
    #[tracing::instrument(skip(self), fields(file_id = %file_id))]
    pub async fn get_file_info_including_deleted(&self, file_id: &str) -> AppResult<FileInfo> {
        let rows = self
            .store()
            .file_info()
            .get_by_ids(std::slice::from_ref(&file_id.to_owned()), true)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "file info lookup failed");
                AppError::boxed(
                    "getFile",
                    "api.file.get_file_info.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        // `len(fileInfos) == 0` is a **404** with the same id as the 500 above it, so a client
        // branching on the id cannot tell a missing file from a broken query.
        rows.into_iter().next().ok_or_else(|| {
            AppError::boxed(
                "getFile",
                "api.file.get_file_info.app_error",
                None,
                String::new(),
                404,
            )
        })
    }

    /// The guard on `generateMiniPreview` (app/file.go:1253), lifted out so both file reads use
    /// one copy of it.
    ///
    /// **All three conditions have to hold**, and the third is what makes the branch rare: the
    /// upload path already generates a preview for every image it accepts (app/file.go:1016), so
    /// a NULL `MiniPreview` on an image means a row written before that code, by a plugin, or by
    /// a direct `INSERT`. An SVG is excluded because it has no raster to sample.
    ///
    /// When the branch *does* fire, Go's own answer depends on whether the file is still in the
    /// backend: present, it returns a freshly encoded preview and persists it; missing, it logs
    /// at debug and returns `mini_preview: null` — which is what we would have returned anyway.
    /// We cannot tell those apart without the backend, so both are forwarded.
    pub(crate) fn mini_preview_would_be_generated(info: &FileInfo) -> bool {
        info.is_image() && !info.is_svg() && info.mini_preview.is_none()
    }
}

/// Port of `app.App.HasPermissionToFileAction` (app/authorization.go:741).
///
/// **Always `true` on this deployment, and the first line of Go's own function is why**:
/// `a.Srv().Channels().AccessControl` is nil unless the attribute-based access control service
/// is registered, and that registration lives in the out-of-scope `enterprise/` tree. The two
/// config gates below it — `AccessControlSettings.EnableAttributeBasedAccessControl` (default
/// `false`, config.go:4090) and `FeatureFlags.PermissionPolicies` — are each independently
/// sufficient to return `true` as well, and only the *third* of the three has a default of
/// `true` (feature_flags.go:172).
///
/// Written as a free function returning a constant rather than as a config read, because there
/// is no configuration reachable from Team Edition that makes it return anything else:
/// modelling `EnableAttributeBasedAccessControl` here would suggest that setting it changes our
/// answer, and it does not — it changes Go's only in the presence of a service we cannot run.
///
/// Called at both of Go's call sites so that the day an ABAC evaluator exists, the two gates are
/// already in the right place.
pub fn has_permission_to_file_action() -> bool {
    true
}

/// The 403 both file routes raise when [`has_permission_to_file_action`] denies.
///
/// Unreachable today, for the reason that function gives. Kept because the two call sites spell
/// the `where` differently — `getFileInfo` against `getFileInfosForPost` — and that is the only
/// thing distinguishing two otherwise identical errors in a log.
pub fn abac_denied(where_: &'static str) -> Box<AppError> {
    AppError::boxed(
        where_,
        "api.file.get_file.abac_denied.app_error",
        None,
        String::new(),
        403,
    )
}

/// The `where` on every `AppError` the file-backend wrappers raise, alongside its message id.
///
/// Split out because Go raises them from *free functions* shared between the two backends, so the
/// `where` names the file-backend function and never says which backend failed — a log line from
/// `ListExportDirectory` and one from `ListDirectory` are identical. Reproduced rather than
/// improved: the string is on the wire in `AppError.request_id`-adjacent logging and in tests.
mod backend_errors {
    /// `fileReader` (app/file.go:139) — used by **both** `FileReader` and `ExportFileReader`.
    pub const FILE_READER: (&str, &str) = ("FileReader", "api.file.file_reader.app_error");
    /// `Server.ReadFile` (app/server.go:1929).
    pub const READ_FILE: (&str, &str) = ("ReadFile", "api.file.read_file.app_error");
    /// `fileExists` (app/file.go:211) — shared by `FileExists` and `ExportFileExists`.
    pub const FILE_EXISTS: (&str, &str) = ("FileExists", "api.file.file_exists.app_error");
    /// `removeFile` (app/file.go:325) — shared by `RemoveFile` and `RemoveExportFile`.
    pub const REMOVE_FILE: (&str, &str) = ("RemoveFile", "api.file.remove_file.app_error");
    /// `listDirectory` (app/file.go:355). **The `where` is `ListExportDirectory` for both**
    /// backends — Go's own copy-paste, kept because changing it would change a log line the Go
    /// server beside us still writes the other way.
    pub const LIST_DIRECTORY: (&str, &str) =
        ("ListExportDirectory", "api.file.list_directory.app_error");
    /// `App.WriteFile` → `writeFile` (app/file.go:266). The `where` is `WriteFile`, not the
    /// caller's name, so an emoji upload that fails the backend write reports the same string a
    /// profile-image write would.
    pub const WRITE_FILE: (&str, &str) = ("WriteFile", "api.file.write_file.app_error");
    /// `App.MoveFile` (app/file.go:249).
    pub const MOVE_FILE: (&str, &str) = ("MoveFile", "api.file.move_file.app_error");
}

impl App {
    /// Turn a backend failure into the answer the route should give.
    ///
    /// Two outcomes, and the split is the whole reason this exists:
    ///
    /// - An **unsupported driver** (S3, Azure) is [`PrepareError::Unreproducible`], so the handler
    ///   forwards to Go and that deployment keeps the answers it has. It is not a 500: nothing is
    ///   wrong, we simply are not the right server for it.
    /// - Anything else is Go's own `AppError`, which for every wrapper in `app/file.go` is a
    ///   **500 with an empty `detailed_error`** — the wrapped cause is discarded there too, so a
    ///   missing file and an unreadable one are the same answer.
    fn backend_error(
        (where_, id): (&'static str, &'static str),
        err: crate::filestore::FileStoreError,
    ) -> PrepareError {
        if err.is_unsupported_driver() {
            return PrepareError::Unreproducible(
                "the configured file backend driver is not implemented here",
            );
        }
        tracing::debug!(error = %err, where_, "file backend operation failed");
        PrepareError::App(AppError::boxed(where_, id, None, String::new(), 500))
    }

    /// Port of `app.App.ReadFile` → `Server.ReadFile` (app/server.go:1929).
    pub async fn read_file(&self, path: &str) -> Result<Vec<u8>, PrepareError> {
        self.file_backend()
            .read_file(path)
            .await
            .map_err(|err| Self::backend_error(backend_errors::READ_FILE, err))
    }

    /// Port of `app.App.FileReader` (app/file.go:174), plus the size `http.ServeContent` learns by
    /// seeking to the end.
    pub async fn file_reader(&self, path: &str) -> Result<(tokio::fs::File, u64), PrepareError> {
        self.file_backend()
            .reader(path)
            .await
            .map_err(|err| Self::backend_error(backend_errors::FILE_READER, err))
    }

    /// Port of `app.App.ExportFileReader` (app/file.go:188).
    pub async fn export_file_reader(
        &self,
        path: &str,
    ) -> Result<(tokio::fs::File, u64), PrepareError> {
        self.export_file_backend()
            .reader(path)
            .await
            .map_err(|err| Self::backend_error(backend_errors::FILE_READER, err))
    }

    /// Port of `app.App.FileExists` (app/file.go:199).
    pub async fn file_exists(&self, path: &str) -> Result<bool, PrepareError> {
        self.file_backend()
            .file_exists(path)
            .await
            .map_err(|err| Self::backend_error(backend_errors::FILE_EXISTS, err))
    }

    /// Port of `app.App.ExportFileExists` (app/file.go:203).
    pub async fn export_file_exists(&self, path: &str) -> Result<bool, PrepareError> {
        self.export_file_backend()
            .file_exists(path)
            .await
            .map_err(|err| Self::backend_error(backend_errors::FILE_EXISTS, err))
    }

    /// Port of `app.App.WriteFile` → `Server.writeFile` (app/file.go:261).
    ///
    /// Go takes an `io.Reader` and streams; this takes the bytes, because every migrated caller
    /// already has them buffered — `createEmoji` reads the whole multipart part into memory to
    /// decode its header, and the part is capped at 512 KiB by `MaxEmojiFileSize`.
    ///
    /// The returned count is the number of bytes written, which no migrated caller reads; it is
    /// kept because Go returns it and discarding it here would hide a short write.
    pub async fn write_file(&self, data: &[u8], path: &str) -> Result<i64, PrepareError> {
        self.file_backend()
            .write_file(data, path)
            .await
            .map_err(|err| Self::backend_error(backend_errors::WRITE_FILE, err))
    }

    /// Port of `app.App.MoveFile` (app/file.go:249).
    pub async fn move_file(&self, old_path: &str, new_path: &str) -> Result<(), PrepareError> {
        self.file_backend()
            .move_file(old_path, new_path)
            .await
            .map_err(|err| Self::backend_error(backend_errors::MOVE_FILE, err))
    }

    /// Port of `app.App.RemoveFile` (app/file.go:315).
    pub async fn remove_file(&self, path: &str) -> Result<(), PrepareError> {
        self.file_backend()
            .remove_file(path)
            .await
            .map_err(|err| Self::backend_error(backend_errors::REMOVE_FILE, err))
    }

    /// Port of `app.App.RemoveExportFile` (app/file.go:319).
    pub async fn remove_export_file(&self, path: &str) -> Result<(), PrepareError> {
        self.export_file_backend()
            .remove_file(path)
            .await
            .map_err(|err| Self::backend_error(backend_errors::REMOVE_FILE, err))
    }

    /// Port of `app.App.ListDirectory` (app/file.go:339) — non-recursive.
    pub async fn list_directory(&self, path: &str) -> Result<Vec<String>, PrepareError> {
        self.file_backend()
            .list_directory(path)
            .await
            .map_err(|err| Self::backend_error(backend_errors::LIST_DIRECTORY, err))
    }

    /// Port of `app.App.ListExportDirectory` (app/file.go:343).
    pub async fn list_export_directory(&self, path: &str) -> Result<Vec<String>, PrepareError> {
        self.export_file_backend()
            .list_directory(path)
            .await
            .map_err(|err| Self::backend_error(backend_errors::LIST_DIRECTORY, err))
    }

    /// Port of `app.App.TestFileStoreConnection` (app/file.go:126).
    ///
    /// The three `FileBackendAuthError` / `FileBackendNoBucketError` arms of
    /// `connectionTestErrorToAppError` (app/file.go:88) are **S3 and Azure only** — the local
    /// backend never constructs either — so a local deployment can reach exactly two answers:
    /// success, or `api.file.test_connection.app_error` at 500 when the directory is not
    /// writable.
    pub async fn test_file_store_connection(&self) -> Result<(), PrepareError> {
        self.file_backend().test_connection().await.map_err(|err| {
            if err.is_unsupported_driver() {
                return PrepareError::Unreproducible(
                    "the configured file backend driver is not implemented here",
                );
            }
            tracing::debug!(error = %err, "file backend connection test failed");
            PrepareError::App(AppError::boxed(
                "TestConnection",
                "api.file.test_connection.app_error",
                None,
                err.to_string(),
                500,
            ))
        })
    }
}

/// Port of `app.GeneratePublicLinkHash` (app/file.go:606).
///
/// SHA-256 over the **salt first, then the file id**, base64 with `RawURLEncoding` — URL-safe
/// alphabet, no padding. All three are easy to get backwards and none of them would show up in a
/// smoke test, because a wrong hash simply makes every public link fail its comparison.
///
/// Pinned by `fixtures/behaviour_filestore.json` (`public_link_hash`), which calls Go's own
/// function.
pub fn generate_public_link_hash(file_id: &str, salt: &str) -> String {
    use base64::Engine;
    use sha2::{Digest, Sha256};

    let mut hash = Sha256::new();
    hash.update(salt.as_bytes());
    hash.update(file_id.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash.finalize())
}

/// Port of `getPublicFile`'s hash check (api4/file.go:918) — `subtle.ConstantTimeCompare`
/// against [`generate_public_link_hash`].
///
/// Constant time is not decoration here. The presented hash is attacker-controlled and the
/// expected one is derived from a server secret; a short-circuiting `==` leaks the length of the
/// matching prefix through timing, which is enough to recover a link hash byte by byte. Go uses
/// `crypto/subtle` for exactly this reason and so does the port.
///
/// `ConstantTimeCompare` returns `0` for inputs of different lengths **without comparing them**,
/// so the length is not protected — in Go either. Reproduced rather than improved.
pub fn public_link_hash_matches(file_id: &str, salt: &str, presented: &str) -> bool {
    use subtle::ConstantTimeEq;

    let expected = generate_public_link_hash(file_id, salt);
    if presented.len() != expected.len() {
        return false;
    }
    presented.as_bytes().ct_eq(expected.as_bytes()).into()
}

#[cfg(test)]
mod public_link_go_parity {
    use super::generate_public_link_hash;

    /// Asserted against `app.GeneratePublicLinkHash` itself.
    ///
    /// The corpus includes a row with the id and salt **swapped**, so a port that hashed them the
    /// other way round produces a hash the fixture does not hold — which is the only way to catch
    /// an operand-order mistake, since both orders are the same length and the same alphabet.
    #[test]
    fn public_link_hash_matches_go() {
        let oracle: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/behaviour_filestore.json"))
                .expect("behaviour_filestore.json is generated by reference/dump");

        let cases = oracle["public_link_hash"].as_array().unwrap();
        assert!(cases.len() >= 6);
        for case in cases {
            let file_id = case["file_id"].as_str().unwrap();
            let salt = case["salt"].as_str().unwrap();
            assert_eq!(
                generate_public_link_hash(file_id, salt),
                case["hash"].as_str().unwrap(),
                "GeneratePublicLinkHash({file_id:?}, {salt:?})"
            );
        }
    }
}
