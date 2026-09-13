//! Port of `server/platform/shared/filestore` — the **local** driver.
//!
//! Go's `FileBackend` is an interface with three implementations: `LocalFileBackend`, an S3 one
//! built on minio-go, and an Azure blob one. Only the local driver is ported, because it is the
//! only one whose behaviour is a property of this code rather than of a remote service: an S3
//! port would be a second client library agreeing with the first, tested against nothing.
//!
//! The other two are therefore [`FileBackend::Unsupported`], which fails every operation with
//! [`FileStoreError::UnsupportedDriver`]. Handlers turn that into a forward to Go — a deployment
//! on S3 keeps the answers it has today, and no route silently serves the wrong bytes.
//!
//! # Every path is `filepath.Join(directory, path)`, and that *cleans*
//!
//! `Join` is lexical: `Join("./data", "../escape")` is `"escape"`, outside the configured
//! directory entirely. Go has the same property and this port reproduces it rather than adding a
//! containment check, because a check would make us answer where Go serves — the divergence a
//! strangler proxy exists to prevent. What keeps it safe is the *router*: every path parameter
//! that reaches a backend read is `[A-Za-z0-9]+` (see `mm_api::segment_matches_go_mux`), so a
//! `..` cannot arrive through a migrated route at all. Pinned by the `join` rows of
//! `fixtures/behaviour_filestore.json`.
//!
//! # `os.IsNotExist` is ENOENT and nothing else
//!
//! The intuitive reading — that a path whose parent is a file (ENOTDIR) is "not found" too — is
//! wrong: `syscall.Errno.Is` maps only `ENOENT` to `ErrNotExist`. So `FileExists("brand/image/x")`
//! where `brand/image` is a file returns an **error**, not `false`. Measured, not reasoned: the
//! oracle case `brand/image/deeper` exists because the first version of this comment said the
//! opposite.
//!
//! Pinned by `fixtures/behaviour_filestore.json` (`local_backend`).

use std::path::PathBuf;

use mm_model::go_path;
use tokio::io::AsyncWriteExt;

/// Port of `filestore.driverLocal` (filesstore.go:18).
pub const DRIVER_LOCAL: &str = "local";
/// Port of `filestore.driverS3` (filesstore.go:17).
pub const DRIVER_S3: &str = "amazons3";
/// Port of `filestore.driverAzure` (filesstore.go:19).
pub const DRIVER_AZURE: &str = "azureblob";

/// Port of `filestore.TestFilePath` (localstore.go:21).
pub const TEST_FILE_PATH: &str = "/testfile";

/// Port of `filestore.MaxRecursionDepth` (localstore.go:22).
pub const MAX_RECURSION_DEPTH: u32 = 50;

/// What `TestConnection` writes and then removes.
const TEST_FILE_CONTENT: &[u8] = b"testingwrite";

/// Errors from the file backend.
///
/// Go wraps every failure with `errors.Wrapf` and a message, and then every caller in `app/file.go`
/// discards that message: the `AppError` it builds carries an empty `detailed_error`. So nothing
/// here needs to reproduce Go's wording — only the *distinction* callers act on, which is
/// [`FileStoreError::is_not_found`].
#[derive(Debug, thiserror::Error)]
pub enum FileStoreError {
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// The configured driver is one this port does not implement (`amazons3`, `azureblob`, or a
    /// value Go's own `FileSettings.isValid` would have rejected).
    #[error("the {driver} file backend is not implemented in this port")]
    UnsupportedDriver { driver: String },
}

impl FileStoreError {
    fn io(operation: &'static str, path: &str, source: std::io::Error) -> Self {
        Self::Io {
            operation,
            path: path.to_owned(),
            source,
        }
    }

    /// Port of `os.IsNotExist` applied to the wrapped error — **ENOENT only**.
    ///
    /// `ErrorKind::NotADirectory` (ENOTDIR) deliberately does not count; see the module docs.
    pub fn is_not_found(&self) -> bool {
        match self {
            Self::Io { source, .. } => source.kind() == std::io::ErrorKind::NotFound,
            Self::UnsupportedDriver { .. } => false,
        }
    }

    /// Whether this is the "we do not implement that driver" case, which handlers turn into a
    /// forward to Go rather than into an error of their own.
    pub fn is_unsupported_driver(&self) -> bool {
        matches!(self, Self::UnsupportedDriver { .. })
    }
}

/// Port of `filestore.FileBackendSettings` (filesstore.go:50), narrowed to the local driver.
///
/// The twenty-eight S3 and Azure fields are not modelled: nothing reads them, and carrying them
/// would suggest this port could act on them. [`FileBackendSettings::driver_name`] keeps the
/// configured value whatever it is, so an unsupported driver is still *named* in a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBackendSettings {
    pub driver_name: String,
    pub directory: String,
}

impl FileBackendSettings {
    /// Port of `NewFileBackendSettingsFromConfig` (filesstore.go:82) for the local branch —
    /// `FileSettings.DriverName` and `FileSettings.Directory`.
    pub fn from_file_settings(driver_name: &str, directory: &str) -> Self {
        Self {
            driver_name: driver_name.to_owned(),
            directory: directory.to_owned(),
        }
    }
}

/// Port of `filestore.FileBackend` (filesstore.go:26), as an enum rather than a trait object.
///
/// An enum because there is exactly one real implementation and one refusal, and a trait would
/// invite a second implementation to be written against nothing. `Unsupported` carries the
/// driver name so the log line says which backend the deployment actually asked for.
#[derive(Debug, Clone)]
pub enum FileBackend {
    Local(LocalFileBackend),
    Unsupported { driver: String },
}

impl FileBackend {
    /// Port of `filestore.NewFileBackend` (filesstore.go:203) → `newFileBackend`.
    ///
    /// Go returns an error for an unknown driver and the server refuses to boot. Here an unknown
    /// driver is [`FileBackend::Unsupported`] instead, because the Go server *is* booting beside
    /// us: a driver it accepted is one we have to route around, not one we can call impossible.
    pub fn new(settings: &FileBackendSettings) -> Self {
        if settings.driver_name == DRIVER_LOCAL {
            Self::Local(LocalFileBackend::new(&settings.directory))
        } else {
            Self::Unsupported {
                driver: settings.driver_name.clone(),
            }
        }
    }

    /// Port of `FileBackend.DriverName`.
    pub fn driver_name(&self) -> &str {
        match self {
            Self::Local(_) => DRIVER_LOCAL,
            Self::Unsupported { driver } => driver,
        }
    }

    fn local(&self) -> Result<&LocalFileBackend, FileStoreError> {
        match self {
            Self::Local(backend) => Ok(backend),
            Self::Unsupported { driver } => Err(FileStoreError::UnsupportedDriver {
                driver: driver.clone(),
            }),
        }
    }

    /// Port of `FileBackend.ReadFile`.
    pub async fn read_file(&self, path: &str) -> Result<Vec<u8>, FileStoreError> {
        self.local()?.read_file(path).await
    }

    /// Port of `FileBackend.Reader`, narrowed to what a range-capable HTTP response needs: the
    /// open handle and the size the seek-to-end in `http.ServeContent` would have produced.
    pub async fn reader(&self, path: &str) -> Result<(tokio::fs::File, u64), FileStoreError> {
        self.local()?.reader(path).await
    }

    /// Port of `FileBackend.FileExists`.
    pub async fn file_exists(&self, path: &str) -> Result<bool, FileStoreError> {
        self.local()?.file_exists(path).await
    }

    /// Port of `FileBackend.FileSize`.
    pub async fn file_size(&self, path: &str) -> Result<i64, FileStoreError> {
        self.local()?.file_size(path).await
    }

    /// Port of `FileBackend.FileModTime`.
    pub async fn file_mod_time(&self, path: &str) -> Result<std::time::SystemTime, FileStoreError> {
        self.local()?.file_mod_time(path).await
    }

    /// Port of `FileBackend.WriteFile`.
    pub async fn write_file(&self, data: &[u8], path: &str) -> Result<i64, FileStoreError> {
        self.local()?.write_file(data, path).await
    }

    /// Port of `FileBackend.RemoveFile`.
    pub async fn remove_file(&self, path: &str) -> Result<(), FileStoreError> {
        self.local()?.remove_file(path).await
    }

    /// Port of `FileBackend.MoveFile`.
    pub async fn move_file(&self, old_path: &str, new_path: &str) -> Result<(), FileStoreError> {
        self.local()?.move_file(old_path, new_path).await
    }

    /// Port of `FileBackend.ListDirectory`.
    pub async fn list_directory(&self, path: &str) -> Result<Vec<String>, FileStoreError> {
        self.local()?.list_directory(path).await
    }

    /// Port of `FileBackend.ListDirectoryRecursively`.
    pub async fn list_directory_recursively(
        &self,
        path: &str,
    ) -> Result<Vec<String>, FileStoreError> {
        self.local()?.list_directory_recursively(path).await
    }

    /// Port of `FileBackend.TestConnection`.
    pub async fn test_connection(&self) -> Result<(), FileStoreError> {
        self.local()?.test_connection().await
    }
}

/// Port of `filestore.LocalFileBackend` (localstore.go:24).
#[derive(Debug, Clone)]
pub struct LocalFileBackend {
    directory: String,
}

impl LocalFileBackend {
    pub fn new(directory: &str) -> Self {
        Self {
            directory: directory.to_owned(),
        }
    }

    /// `filepath.Join(b.directory, path)` — see the module docs on why this cleans rather than
    /// contains.
    fn resolve(&self, path: &str) -> PathBuf {
        PathBuf::from(go_path::join(&[&self.directory, path]))
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, FileStoreError> {
        let full = self.resolve(path);
        tokio::fs::read(&full)
            .await
            .map_err(|err| FileStoreError::io("read", path, err))
    }

    /// `os.Open` plus the `Seek(0, io.SeekEnd)` that `http.ServeContent` performs to learn the
    /// size. Done here with `metadata()` on the already-open handle, so the size describes the
    /// file we are about to read and not a different one that replaced it between two syscalls.
    async fn reader(&self, path: &str) -> Result<(tokio::fs::File, u64), FileStoreError> {
        let full = self.resolve(path);
        let file = tokio::fs::File::open(&full)
            .await
            .map_err(|err| FileStoreError::io("open", path, err))?;
        let size = file
            .metadata()
            .await
            .map_err(|err| FileStoreError::io("stat", path, err))?
            .len();
        Ok((file, size))
    }

    async fn file_exists(&self, path: &str) -> Result<bool, FileStoreError> {
        let full = self.resolve(path);
        match tokio::fs::metadata(&full).await {
            Ok(_) => Ok(true),
            // `os.IsNotExist(err)` — ENOENT only. ENOTDIR falls through to the error arm, as it
            // does in Go.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(FileStoreError::io("stat", path, err)),
        }
    }

    async fn file_size(&self, path: &str) -> Result<i64, FileStoreError> {
        let full = self.resolve(path);
        let meta = tokio::fs::metadata(&full)
            .await
            .map_err(|err| FileStoreError::io("stat", path, err))?;
        // `os.FileInfo.Size()` is an int64 and Go returns it unchanged. A file larger than
        // i64::MAX cannot exist on any filesystem this runs on; saturating rather than
        // `as`-casting says that out loud instead of wrapping to a negative size.
        Ok(i64::try_from(meta.len()).unwrap_or(i64::MAX))
    }

    async fn file_mod_time(&self, path: &str) -> Result<std::time::SystemTime, FileStoreError> {
        let full = self.resolve(path);
        let meta = tokio::fs::metadata(&full)
            .await
            .map_err(|err| FileStoreError::io("stat", path, err))?;
        meta.modified()
            .map_err(|err| FileStoreError::io("modtime", path, err))
    }

    /// Port of `writeFileLocally` (localstore.go:150): `MkdirAll(Dir(path), 0750)` then
    /// `O_WRONLY|O_CREATE|O_TRUNC` at mode 0600.
    ///
    /// The two modes differ and both matter: a directory the Go server has to traverse needs the
    /// execute bit, and a file it has to read back needs neither group nor other access.
    async fn write_file(&self, data: &[u8], path: &str) -> Result<i64, FileStoreError> {
        let full = self.resolve(path);
        let parent = go_path::dir(&full.to_string_lossy());
        create_dir_all_mode(&parent, 0o750)
            .await
            .map_err(|err| FileStoreError::io("mkdir", path, err))?;

        let mut file = open_write_truncate_mode(&full, 0o600)
            .await
            .map_err(|err| FileStoreError::io("create", path, err))?;
        file.write_all(data)
            .await
            .map_err(|err| FileStoreError::io("write", path, err))?;
        file.flush()
            .await
            .map_err(|err| FileStoreError::io("write", path, err))?;
        Ok(i64::try_from(data.len()).unwrap_or(i64::MAX))
    }

    async fn remove_file(&self, path: &str) -> Result<(), FileStoreError> {
        let full = self.resolve(path);
        tokio::fs::remove_file(&full)
            .await
            .map_err(|err| FileStoreError::io("remove", path, err))
    }

    /// Port of `LocalFileBackend.MoveFile` (localstore.go:139): `MkdirAll(Dir(new), 0750)` then
    /// `os.Rename`.
    ///
    /// **It is a rename, not a copy**, so it fails across filesystems (EXDEV) where a copy would
    /// not — and it does not create the *source*'s parent, only the destination's. The only
    /// caller that reaches it from a migrated route is `deleteEmojiImage`, which moves
    /// `emoji/<id>/image` to `emoji/<id>/image_deleted` inside one directory, so the `MkdirAll` is
    /// a no-op there and the rename cannot cross a device.
    ///
    /// A missing source is an ordinary error, **not** a distinguished not-found: `App.MoveFile`
    /// flattens every failure into one 500 and `deleteEmojiImage` only logs it.
    async fn move_file(&self, old_path: &str, new_path: &str) -> Result<(), FileStoreError> {
        let new_full = self.resolve(new_path);
        let parent = go_path::dir(&new_full.to_string_lossy());
        create_dir_all_mode(&parent, 0o750)
            .await
            .map_err(|err| FileStoreError::io("mkdir", new_path, err))?;

        tokio::fs::rename(self.resolve(old_path), &new_full)
            .await
            .map_err(|err| FileStoreError::io("rename", new_path, err))
    }

    /// Port of `LocalFileBackend.ListDirectory` (localstore.go:228).
    ///
    /// Three properties the oracle pins, none of them obvious:
    ///
    /// 1. Entries are `filepath.Join(path, name)` — **prefixed with the argument**, never with the
    ///    backend directory. So `list_directory("export")` answers `["export/job.zip", …]`.
    /// 2. A path that does not exist is an **empty list and no error**. Go's own comment says it
    ///    should have been `os.ErrNotExist` and was left as is for consistency.
    /// 3. That leniency is ENOENT only. Listing a *file* is ENOTDIR and does return an error.
    async fn list_directory(&self, path: &str) -> Result<Vec<String>, FileStoreError> {
        let full = self.resolve(path);
        let mut entries = match tokio::fs::read_dir(&full).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(FileStoreError::io("readdir", path, err)),
        };

        let mut results = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|err| FileStoreError::io("readdir", path, err))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            results.push(go_path::join(&[path, &name]));
        }
        Ok(results)
    }

    /// Port of `LocalFileBackend.ListDirectoryRecursively` → `appendRecursively`
    /// (localstore.go:193).
    ///
    /// Files only — a directory contributes its contents and not itself — **except** at the depth
    /// cap, where the directory is pushed as a result and not descended into. Go decrements from
    /// [`MAX_RECURSION_DEPTH`] and tests `maxDepth <= 0`, so 50 levels of directory are walked
    /// and the 51st is emitted as a path.
    async fn list_directory_recursively(&self, path: &str) -> Result<Vec<String>, FileStoreError> {
        let mut results = Vec::new();
        self.append_recursively(path, MAX_RECURSION_DEPTH, &mut results)
            .await?;
        Ok(results)
    }

    /// The recursion, written as an explicit stack because an `async fn` cannot recurse without
    /// boxing its future — and a boxed future per directory level is a cost paid on every listing
    /// to satisfy the compiler rather than the algorithm.
    ///
    /// The traversal order is Go's: depth-first, in `os.ReadDir` order (which sorts by filename),
    /// with each directory's results spliced in where the directory sat. The stack therefore
    /// holds *pending* entries in reverse.
    async fn append_recursively(
        &self,
        base: &str,
        max_depth: u32,
        results: &mut Vec<String>,
    ) -> Result<(), FileStoreError> {
        // (path relative to the backend directory, remaining depth)
        let mut stack: Vec<(String, u32)> = vec![(base.to_owned(), max_depth)];

        while let Some((path, depth)) = stack.pop() {
            let full = self.resolve(&path);
            let mut entries = match tokio::fs::read_dir(&full).await {
                Ok(entries) => entries,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(FileStoreError::io("readdir", &path, err)),
            };

            // `os.ReadDir` sorts by filename; `read_dir` does not, so the level is collected and
            // sorted before anything is emitted. Without this the recursive listing would come
            // back in inode order and no two runs would agree.
            let mut level: Vec<(String, bool)> = Vec::new();
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|err| FileStoreError::io("readdir", &path, err))?
            {
                let name = entry.file_name().to_string_lossy().into_owned();
                let is_dir = entry
                    .file_type()
                    .await
                    .map_err(|err| FileStoreError::io("filetype", &path, err))?
                    .is_dir();
                level.push((name, is_dir));
            }
            level.sort_by(|a, b| a.0.cmp(&b.0));

            // Go skips `.`, `..` and any entry whose joined path equals the directory itself.
            // `read_dir` never yields the first two; the third is kept because it is the guard
            // that stops `list_directory_recursively("")` from walking into itself.
            let mut pending = Vec::new();
            for (name, is_dir) in level {
                let entry_path = go_path::join(&[&path, &name]);
                if name == "." || name == ".." || entry_path == path {
                    continue;
                }
                if is_dir {
                    if depth == 0 {
                        tracing::warn!(
                            depth,
                            path = %entry_path,
                            "max depth reached, skipping any further directories"
                        );
                        results.push(entry_path);
                        continue;
                    }
                    pending.push((entry_path, depth - 1));
                } else {
                    results.push(entry_path);
                }
            }

            // Depth-first in listing order: push in reverse so the first pending directory is the
            // next one popped.
            for item in pending.into_iter().rev() {
                stack.push(item);
            }
        }

        Ok(())
    }

    /// Port of `LocalFileBackend.TestConnection` (localstore.go:76).
    ///
    /// Writes twelve bytes to `/testfile` and removes it. **The removal's error is discarded** in
    /// Go — `os.Remove` is called without checking — so a backend that can write but not unlink
    /// still reports a healthy connection and leaves the file behind.
    async fn test_connection(&self) -> Result<(), FileStoreError> {
        self.write_file(TEST_FILE_CONTENT, TEST_FILE_PATH).await?;
        let _ = self.remove_file(TEST_FILE_PATH).await;
        tracing::debug!("Able to write files to local storage.");
        Ok(())
    }
}

/// `os.MkdirAll(dir, 0750)`. `tokio::fs::DirBuilder` carries the mode, which `create_dir_all`
/// alone does not — and the mode is the reason Go passes 0750 rather than `os.ModePerm`.
///
/// `mode` is applied only to directories this call creates, exactly as `MkdirAll` does; an
/// existing directory keeps whatever permissions it has.
async fn create_dir_all_mode(dir: &str, mode: u32) -> std::io::Result<()> {
    // `Dir` of a bare filename is `"."`, which already exists; Go's MkdirAll is a no-op there too.
    tokio::fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(dir)
        .await
}

/// `os.OpenFile(path, O_WRONLY|O_CREATE|O_TRUNC, 0600)`.
async fn open_write_truncate_mode(
    path: &std::path::Path,
    mode: u32,
) -> std::io::Result<tokio::fs::File> {
    tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .await
}

#[cfg(test)]
mod go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_filestore.json"))
            .expect("behaviour_filestore.json is generated by reference/dump")
    }

    /// Rebuild the oracle's tree under a fresh temporary root and hand back a backend over it.
    ///
    /// The tree is read *from the fixture* rather than restated here, so the Rust side cannot
    /// drift from what Go was measured against: change the corpus in `behaviour_filestore.go`,
    /// regenerate, and these tests test the new tree.
    async fn backend_over_oracle_tree() -> (FileBackend, tempdir::TempDir) {
        let oracle = oracle();
        let root = tempdir::TempDir::new();
        let tree = oracle["local_backend"]["tree"].as_object().unwrap();
        for (name, content) in tree {
            let full = root.path().join(name);
            tokio::fs::create_dir_all(full.parent().unwrap())
                .await
                .unwrap();
            tokio::fs::write(&full, content.as_str().unwrap().as_bytes())
                .await
                .unwrap();
        }
        let backend = FileBackend::new(&FileBackendSettings::from_file_settings(
            DRIVER_LOCAL,
            &root.path().to_string_lossy(),
        ));
        (backend, root)
    }

    #[tokio::test]
    async fn driver_name_matches_go() {
        let oracle = oracle();
        let (backend, _root) = backend_over_oracle_tree().await;
        assert_eq!(
            backend.driver_name(),
            oracle["local_backend"]["driver_name"].as_str().unwrap()
        );
    }

    /// `ReadFile`, `FileExists` and `FileSize` over present, absent, directory, rooted, dotted,
    /// escaping and ENOTDIR paths — Go's answer for each.
    #[tokio::test]
    async fn reads_match_go() {
        let oracle = oracle();
        let (backend, _root) = backend_over_oracle_tree().await;

        let cases = oracle["local_backend"]["reads"].as_array().unwrap();
        assert!(cases.len() >= 10, "corpus should cover the read branches");
        for case in cases {
            let path = case["path"].as_str().unwrap();

            match backend.read_file(path).await {
                Ok(bytes) => {
                    assert!(
                        case["read_ok"].as_bool().unwrap(),
                        "read_file({path:?}) should have failed"
                    );
                    assert_eq!(
                        String::from_utf8(bytes).unwrap(),
                        case["read_bytes"].as_str().unwrap(),
                        "read_file({path:?})"
                    );
                }
                Err(err) => {
                    assert!(
                        !case["read_ok"].as_bool().unwrap(),
                        "read_file({path:?}) should have succeeded, got {err}"
                    );
                }
            }

            match backend.file_exists(path).await {
                Ok(exists) => {
                    assert!(
                        case["exists_ok"].as_bool().unwrap(),
                        "file_exists({path:?}) should have failed"
                    );
                    assert_eq!(
                        exists,
                        case["exists"].as_bool().unwrap(),
                        "file_exists({path:?})"
                    );
                }
                Err(err) => {
                    assert!(
                        !case["exists_ok"].as_bool().unwrap(),
                        "file_exists({path:?}) should have succeeded, got {err}"
                    );
                }
            }

            match backend.file_size(path).await {
                Ok(size) => {
                    assert!(
                        case["size_ok"].as_bool().unwrap(),
                        "file_size({path:?}) should have failed"
                    );
                    // A directory's size is the filesystem's block size — 4096 for the oracle's
                    // ext4 and for the CI container's overlayfs, but not a property of this port.
                    // Only regular files are compared against Go's number.
                    if case["read_ok"].as_bool().unwrap() {
                        assert_eq!(size, case["size"].as_i64().unwrap(), "file_size({path:?})");
                    }
                }
                Err(err) => {
                    assert!(
                        !case["size_ok"].as_bool().unwrap(),
                        "file_size({path:?}) should have succeeded, got {err}"
                    );
                }
            }
        }
    }

    /// The escape is real and this is the test that says so: `../etc/passwd` resolves *outside*
    /// the backend directory. Go behaves identically, which is why the port does not add a
    /// containment check — see the module docs.
    #[tokio::test]
    async fn a_dotdot_path_leaves_the_backend_directory() {
        let root = tempdir::TempDir::new();
        let inner = root.path().join("data");
        tokio::fs::create_dir_all(&inner).await.unwrap();
        tokio::fs::write(root.path().join("outside.txt"), b"escaped")
            .await
            .unwrap();

        let backend = FileBackend::new(&FileBackendSettings::from_file_settings(
            DRIVER_LOCAL,
            &inner.to_string_lossy(),
        ));
        assert_eq!(
            backend.read_file("../outside.txt").await.unwrap(),
            b"escaped".to_vec()
        );
    }

    /// `ListDirectory` and `ListDirectoryRecursively`: the argument prefix, the empty answer for
    /// a missing path, and the error for a path that is a file.
    #[tokio::test]
    async fn listings_match_go() {
        let oracle = oracle();
        let (backend, _root) = backend_over_oracle_tree().await;

        for case in oracle["local_backend"]["lists"].as_array().unwrap() {
            let path = case["path"].as_str().unwrap();

            match backend.list_directory(path).await {
                Ok(mut got) => {
                    assert!(
                        case["list_ok"].as_bool().unwrap(),
                        "list_directory({path:?}) should have failed"
                    );
                    got.sort();
                    let want: Vec<&str> = case["list"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap())
                        .collect();
                    assert_eq!(got, want, "list_directory({path:?})");
                }
                Err(err) => assert!(
                    !case["list_ok"].as_bool().unwrap(),
                    "list_directory({path:?}) should have succeeded, got {err}"
                ),
            }

            match backend.list_directory_recursively(path).await {
                Ok(mut got) => {
                    assert!(
                        case["list_recursive_ok"].as_bool().unwrap(),
                        "list_directory_recursively({path:?}) should have failed"
                    );
                    got.sort();
                    let want: Vec<&str> = case["list_recursive"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap())
                        .collect();
                    assert_eq!(got, want, "list_directory_recursively({path:?})");
                }
                Err(err) => assert!(
                    !case["list_recursive_ok"].as_bool().unwrap(),
                    "list_directory_recursively({path:?}) should have succeeded, got {err}"
                ),
            }
        }
    }

    /// The recursive listing is depth-first in `os.ReadDir` order, not the sorted flattening the
    /// `listings_match_go` comparison would also accept — that one sorts both sides so it cannot
    /// see order at all. Asserted unsorted against the tree the oracle uses.
    #[tokio::test]
    async fn recursive_listing_is_depth_first_in_name_order() {
        let (backend, _root) = backend_over_oracle_tree().await;
        assert_eq!(
            backend.list_directory_recursively("").await.unwrap(),
            vec![
                "brand/image",
                "emoji/abc/image",
                "export/job.zip",
                "export/nested/deep.zip",
                "users/uid/profile.png",
            ]
        );
    }

    /// `TestConnection` succeeds and leaves nothing behind.
    #[tokio::test]
    async fn test_connection_matches_go() {
        let oracle = oracle();
        let (backend, _root) = backend_over_oracle_tree().await;

        assert_eq!(
            backend.test_connection().await.is_ok(),
            oracle["local_backend"]["test_connection"]["ok"]
                .as_bool()
                .unwrap()
        );
        assert_eq!(
            backend.file_exists(TEST_FILE_PATH).await.unwrap(),
            oracle["local_backend"]["test_connection"]["testfile_leftover"]
                .as_bool()
                .unwrap()
        );
    }

    /// `WriteFile` creates the parent directory and truncates an existing file, and the two modes
    /// are the ones `writeFileLocally` passes.
    #[tokio::test]
    async fn write_file_creates_parents_and_truncates() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir::TempDir::new();
        let backend = FileBackend::new(&FileBackendSettings::from_file_settings(
            DRIVER_LOCAL,
            &root.path().to_string_lossy(),
        ));

        assert_eq!(
            backend
                .write_file(b"a longer first write", "deep/nested/f")
                .await
                .unwrap(),
            20
        );
        assert_eq!(
            backend.write_file(b"short", "deep/nested/f").await.unwrap(),
            5
        );
        assert_eq!(
            backend.read_file("deep/nested/f").await.unwrap(),
            b"short".to_vec()
        );

        let file_mode = tokio::fs::metadata(root.path().join("deep/nested/f"))
            .await
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600, "O_CREATE mode from writeFileLocally");

        let dir_mode = tokio::fs::metadata(root.path().join("deep/nested"))
            .await
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o750, "MkdirAll mode from writeFileLocally");
    }

    /// Every operation on an S3 or Azure deployment refuses in a way handlers can recognise, so
    /// the request is forwarded to Go rather than answered wrongly.
    #[tokio::test]
    async fn an_unsupported_driver_refuses_rather_than_guessing() {
        for driver in [DRIVER_S3, DRIVER_AZURE, "", "something-new"] {
            let backend =
                FileBackend::new(&FileBackendSettings::from_file_settings(driver, "./data"));
            let err = backend.read_file("brand/image").await.unwrap_err();
            assert!(
                err.is_unsupported_driver(),
                "{driver} should be unsupported"
            );
            assert!(!err.is_not_found());
            assert_eq!(backend.driver_name(), driver);
        }
    }

    /// A missing file is `is_not_found`; a path whose parent is a *file* (ENOTDIR) is not. This
    /// is the distinction `GetTeamIcon` turns into 404-vs-500, and the oracle case
    /// `brand/image/deeper` is why the port tests only `ErrorKind::NotFound`.
    #[tokio::test]
    async fn not_found_is_enoent_only() {
        let (backend, _root) = backend_over_oracle_tree().await;

        let missing = backend.read_file("missing/file").await.unwrap_err();
        assert!(missing.is_not_found());

        let not_a_dir = backend.read_file("brand/image/deeper").await.unwrap_err();
        assert!(!not_a_dir.is_not_found());
    }

    /// A minimal scoped temporary directory. `tempfile` is not in the tree and this needs six
    /// lines, not a dependency.
    mod tempdir {
        pub struct TempDir(std::path::PathBuf);

        impl TempDir {
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self {
                // The process id and a counter are enough: tests within a process are
                // distinguished by the counter, and processes by the pid.
                use std::sync::atomic::{AtomicU64, Ordering};
                static NEXT: AtomicU64 = AtomicU64::new(0);
                let path = std::env::temp_dir().join(format!(
                    "mmrs-filestore-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                let _ = std::fs::remove_dir_all(&path);
                std::fs::create_dir_all(&path).expect("temp dir");
                Self(path)
            }

            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
