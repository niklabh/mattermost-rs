//! Installing and removing plugin bundles (app/plugin_install.go, app/extract_plugin_tar.go, and
//! `syncPlugins`/`getPluginsFromFolder` in app/plugin.go).
//!
//! A bundle is a `.tar.gz`. It is extracted to a temporary directory, its manifest found, and the
//! plugin directory copied to `<PluginSettings.Directory>/<id>`; a webapp is unpacked beside it,
//! and the plugin is activated when `PluginStates` enables it. An install also keeps the bundle in
//! the file store as `plugins/<id>.tar.gz`, which is what a server syncs from when it starts.
//!
//! # Extraction keeps Go's path check, weakness included
//!
//! `extractTarGz` joins each name onto the destination and refuses the entry only when the result
//! does not **start with** the destination as a string. `../outx/f` against a destination `…/out`
//! passes, and is written beside it. The temporary directory's random suffix is what stands
//! between that and a real escape, in Go and here alike; `plugin_install::go_parity` runs Go's own
//! function over the same corpus, the sibling case included.
//!
//! # Not ported
//!
//! - Signatures (`plugin_signature.go`). With `RequirePluginSignature` on, a synced bundle is
//!   skipped with an error, where Go would verify it — [D-811].
//! - The cluster messages (`notifyClusterPluginEvent`): there is no cluster.
//! - `unregisterPluginCommands`: no plugin can register a command here yet (the API is Phase 6).

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};

use mm_model::go_path;
use mm_model::manifest::{
    MAX_ID_LENGTH, MIN_ID_LENGTH, Manifest, VALID_ID_REGEX, is_valid_plugin_id,
};
use mm_model::utils::AppError;
use mm_plugin::environment::find_manifest;

use crate::App;

/// Go's `fileStorePluginFolder`.
const FILE_STORE_PLUGIN_FOLDER: &str = "plugins";

/// What to do when a plugin with the same id is already installed (plugin_install.go:351).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStrategy {
    /// Refuse with `app.plugin.install_id.app_error`.
    OnlyIfNew,
    /// Replace only an older version; otherwise `app.plugin.skip_installation.app_error`.
    OnlyIfNewOrUpgrade,
    /// Replace whatever is there.
    Always,
}

/// Why `extractTarGz` failed. The text is Go's where Go's is fixed; an I/O failure carries the
/// operating system's.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExtractError {
    #[error("no destination path provided")]
    NoDestination,
    #[error("failed to initialize gzip reader: {0}")]
    Gzip(std::io::Error),
    #[error("failed to read next file from archive: {0}")]
    Next(std::io::Error),
    #[error("failed to sanitize path {0}")]
    Sanitize(String),
    #[error("{op} {path}: {source}")]
    Io {
        op: &'static str,
        path: String,
        source: std::io::Error,
    },
}

fn io_err<'a>(op: &'static str, path: &'a str) -> impl FnOnce(std::io::Error) -> ExtractError + 'a {
    move |source| ExtractError::Io {
        op,
        path: path.to_owned(),
        source,
    }
}

/// Port of `extractTarGz` (extract_plugin_tar.go:21): unpack a gzipped tar into `dst`.
///
/// Only directories and regular files are extracted; every other entry (links, devices, FIFOs,
/// global PAX headers) is skipped. A directory is made with mode 0744 whatever the archive says,
/// and only its own level — a missing parent is an error. A file makes its parents (0744) and is
/// written with its permission bits, so setuid and the like are dropped. The first failure ends
/// the extraction with what came before it left in place, a partly written file included.
pub fn extract_tar_gz(gzip_stream: &[u8], dst: &str) -> Result<(), ExtractError> {
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};

    if dst.is_empty() {
        return Err(ExtractError::NoDestination);
    }
    let mut decoder = flate2::read::MultiGzDecoder::new(gzip_stream);
    // Go's `gzip.NewReader` reads the header before anything else; a first read does the same.
    let mut first = [0u8; 0];
    decoder.read(&mut first).map_err(ExtractError::Gzip)?;
    let mut archive = tar::Archive::new(decoder);
    let entries = archive.entries().map_err(ExtractError::Next)?;
    for entry in entries {
        let mut entry = entry.map_err(ExtractError::Next)?;
        let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let raw = entry.header().as_old().linkflag[0];
        // Go's reader turns the legacy '\x00' into a directory when the name ends in a slash.
        let is_dir = match entry.header().entry_type() {
            tar::EntryType::Directory => true,
            tar::EntryType::Regular => raw == 0 && name.ends_with('/'),
            _ => false,
        };
        let is_file = !is_dir && entry.header().entry_type() == tar::EntryType::Regular;
        if !is_dir && !is_file {
            tracing::warn!(
                header_type = %char::from(raw),
                header_name = %name,
                "skipping unsupported header type on extracting tar file"
            );
            continue;
        }

        let path = go_path::join(&[dst, &name]);
        if !path.starts_with(dst) {
            return Err(ExtractError::Sanitize(name));
        }

        if is_dir {
            match std::fs::DirBuilder::new().mode(0o744).create(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(io_err("mkdir", &path)(e)),
            }
            continue;
        }
        let dir = go_path::dir(&path);
        std::fs::DirBuilder::new()
            .mode(0o744)
            .recursive(true)
            .create(&dir)
            .map_err(io_err("mkdir", &dir))?;
        let mode = entry.header().mode().unwrap_or(0) & 0o777;
        let mut out = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&path)
            .map_err(io_err("open", &path))?;
        std::io::copy(&mut entry, &mut out).map_err(io_err("write", &path))?;
    }
    Ok(())
}

fn plugin_error(
    where_: &str,
    id: &str,
    status: i32,
    err: impl std::error::Error + Send + Sync + 'static,
) -> Box<AppError> {
    Box::new(AppError::new(where_, id, None, "", status).wrap(err))
}

/// Port of `extractPlugin` (plugin_install.go:392): extract, step into a lone top-level
/// directory, and read and check the manifest.
pub fn extract_plugin(
    bundle: &[u8],
    extract_dir: &Path,
) -> Result<(Manifest, PathBuf), Box<AppError>> {
    let dst = extract_dir.to_string_lossy().into_owned();
    extract_tar_gz(bundle, &dst)
        .map_err(|e| plugin_error("extractPlugin", "app.plugin.extract.app_error", 400, e))?;

    let entries: Vec<std::fs::DirEntry> = std::fs::read_dir(extract_dir)
        .and_then(|d| d.collect::<Result<_, _>>())
        .map_err(|e| plugin_error("extractPlugin", "app.plugin.filesystem.app_error", 500, e))?;
    let mut dir = extract_dir.to_path_buf();
    if let [only] = entries.as_slice()
        && only.file_type().is_ok_and(|t| t.is_dir())
    {
        dir = dir.join(only.file_name());
    }

    let (manifest, _, err) = find_manifest(&dir);
    let manifest = match (manifest, err) {
        (Some(manifest), None) => manifest,
        (_, Some(err)) => {
            return Err(plugin_error(
                "extractPlugin",
                "app.plugin.manifest.app_error",
                400,
                err,
            ));
        }
        (None, None) => {
            return Err(AppError::boxed(
                "extractPlugin",
                "app.plugin.manifest.app_error",
                None,
                "",
                400,
            ));
        }
    };
    if !is_valid_plugin_id(&manifest.id) {
        let params = HashMap::from([
            ("Min".to_owned(), serde_json::json!(MIN_ID_LENGTH)),
            ("Max".to_owned(), serde_json::json!(MAX_ID_LENGTH)),
            ("Regex".to_owned(), serde_json::json!(VALID_ID_REGEX)),
        ]);
        return Err(AppError::boxed(
            "extractPlugin",
            "app.plugin.invalid_id.app_error",
            Some(params),
            "",
            400,
        ));
    }
    Ok((manifest, dir))
}

/// Go's `getBundleStorePath`.
pub fn bundle_store_path(id: &str) -> String {
    go_path::join(&[FILE_STORE_PLUGIN_FOLDER, &format!("{id}.tar.gz")])
}

/// Go's `getSignatureStorePath`.
pub fn signature_store_path(id: &str) -> String {
    go_path::join(&[FILE_STORE_PLUGIN_FOLDER, &format!("{id}.tar.gz.sig")])
}

/// A bundle in the file store and its signature, when there is one (app/plugin.go,
/// `pluginSignaturePath`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSignaturePath {
    pub plugin_id: String,
    pub bundle_path: String,
    pub signature_path: String,
}

/// Port of `getPluginsFromFilePaths` (app/plugin.go:871): every `.tar.gz` by id, then each
/// `.tar.gz.sig` attached to its bundle. A signature without a bundle is logged and dropped.
pub fn plugins_from_file_paths(paths: &[String]) -> BTreeMap<String, PluginSignaturePath> {
    let mut found = BTreeMap::new();
    for path in paths {
        if let Some(id) = go_path::base(path).strip_suffix(".tar.gz") {
            found.insert(
                id.to_owned(),
                PluginSignaturePath {
                    plugin_id: id.to_owned(),
                    bundle_path: path.clone(),
                    signature_path: String::new(),
                },
            );
        }
    }
    for path in paths {
        if let Some(id) = go_path::base(path).strip_suffix(".tar.gz.sig") {
            match found.get_mut(id) {
                Some(bundle) => bundle.signature_path = path.clone(),
                None => tracing::warn!(path = %path, "Unknown signature"),
            }
        }
    }
    found
}

/// A strict semantic version, or `app.plugin.invalid_version.app_error` with `status`.
fn strict_version(
    version: &str,
    status: i32,
) -> Result<mm_model::manifest::StrictVersion, Box<AppError>> {
    mm_model::manifest::StrictVersion::parse_detailed(version).map_err(|e| {
        plugin_error(
            "installExtractedPlugin",
            "app.plugin.invalid_version.app_error",
            status,
            e,
        )
    })
}

impl App {
    /// Port of `App.InstallPlugin` (plugin_install.go:172): `replace` decides whether an
    /// installed plugin with the same id is refused or replaced.
    pub async fn install_plugin(
        &self,
        bundle: &[u8],
        replace: bool,
    ) -> Result<Option<Manifest>, Box<AppError>> {
        let strategy = if replace {
            InstallStrategy::Always
        } else {
            InstallStrategy::OnlyIfNew
        };
        self.install_plugin_with(bundle, None, strategy).await
    }

    /// Port of `Channels.installPlugin` (plugin_install.go:186): install locally, keep the
    /// bundle (and signature) in the file store, and tell clients.
    pub async fn install_plugin_with(
        &self,
        bundle: &[u8],
        signature: Option<&[u8]>,
        strategy: InstallStrategy,
    ) -> Result<Option<Manifest>, Box<AppError>> {
        let manifest = self.install_plugin_locally(bundle, strategy).await?;
        self.install_plugin_to_filestore(&manifest, bundle, signature)
            .await?;
        if let Some(environment) = self.plugins_environment() {
            self.notify_plugin_enabled(&environment, &manifest).await;
        }
        self.notify_plugin_statuses_changed().await;
        Ok(Some(manifest))
    }

    /// Port of `installPluginToFilestore` (plugin_install.go:214).
    async fn install_plugin_to_filestore(
        &self,
        manifest: &Manifest,
        bundle: &[u8],
        signature: Option<&[u8]>,
    ) -> Result<(), Box<AppError>> {
        tracing::info!(plugin_id = %manifest.id, "Persisting plugin to filestore");
        match signature {
            None => tracing::warn!(
                plugin_id = %manifest.id,
                "No signature when persisting plugin to filestore"
            ),
            Some(signature) => {
                self.file_backend()
                    .write_file(signature, &signature_store_path(&manifest.id))
                    .await
                    .map_err(|e| {
                        plugin_error(
                            "saveSignature",
                            "app.plugin.store_signature.app_error",
                            500,
                            e,
                        )
                    })?;
            }
        }
        self.file_backend()
            .write_file(bundle, &bundle_store_path(&manifest.id))
            .await
            .map_err(|e| {
                plugin_error("uploadPlugin", "app.plugin.store_bundle.app_error", 500, e)
            })?;
        Ok(())
    }

    /// Port of `installPluginLocally` (plugin_install.go:366).
    pub async fn install_plugin_locally(
        &self,
        bundle: &[u8],
        strategy: InstallStrategy,
    ) -> Result<Manifest, Box<AppError>> {
        if self.plugins_environment().is_none() {
            return Err(AppError::boxed(
                "installPluginLocally",
                "app.plugin.disabled.app_error",
                None,
                "",
                501,
            ));
        }
        let tmp = TempDir::new().map_err(|e| {
            plugin_error(
                "installPluginLocally",
                "app.plugin.filesystem.app_error",
                500,
                e,
            )
        })?;
        let (manifest, plugin_dir) = extract_plugin(bundle, tmp.path())?;
        self.install_extracted_plugin(manifest, &plugin_dir, strategy)
            .await
    }

    /// Port of `installExtractedPlugin` (plugin_install.go:427).
    async fn install_extracted_plugin(
        &self,
        manifest: Manifest,
        from_plugin_dir: &Path,
        strategy: InstallStrategy,
    ) -> Result<Manifest, Box<AppError>> {
        const WHERE: &str = "installExtractedPlugin";
        tracing::info!(plugin_id = %manifest.id, version = %manifest.version, "Installing extracted plugin");
        let Some(environment) = self.plugins_environment() else {
            return Err(AppError::boxed(
                WHERE,
                "app.plugin.disabled.app_error",
                None,
                "",
                501,
            ));
        };
        let bundles = environment
            .available()
            .map_err(|e| plugin_error(WHERE, "app.plugin.install.app_error", 500, e))?;
        let existing = bundles
            .into_iter()
            .filter_map(|b| b.manifest)
            .find(|m| m.id == manifest.id);

        if let Some(existing) = existing {
            match strategy {
                InstallStrategy::OnlyIfNew => {
                    return Err(AppError::boxed(
                        WHERE,
                        "app.plugin.install_id.app_error",
                        None,
                        "",
                        400,
                    ));
                }
                InstallStrategy::OnlyIfNewOrUpgrade => {
                    let version = strict_version(&manifest.version, 400)?;
                    let existing_version = strict_version(&existing.version, 500)?;
                    if version <= existing_version {
                        tracing::warn!(
                            plugin_id = %manifest.id,
                            version = %manifest.version,
                            existing_version = %existing.version,
                            "Skipping local installation of plugin since not a newer version"
                        );
                        let params =
                            HashMap::from([("Id".to_owned(), serde_json::json!(manifest.id))]);
                        return Err(AppError::boxed(
                            WHERE,
                            "app.plugin.skip_installation.app_error",
                            Some(params),
                            "",
                            500,
                        ));
                    }
                }
                InstallStrategy::Always => {}
            }
            tracing::info!(
                plugin_id = %manifest.id,
                existing_version = %existing.version,
                "Removing existing installation of plugin before local install"
            );
            if let Err(err) = self.remove_plugin_locally(&existing.id).await {
                return Err(Box::new(
                    AppError::new(
                        WHERE,
                        "app.plugin.install_id_failed_remove.app_error",
                        None,
                        "",
                        500,
                    )
                    .wrap(*err),
                ));
            }
        }

        let config = self.config();
        let bundle_path = go_path::join(&[&config.plugin_directory, &manifest.id]);
        mm_plugin::environment::copy_dir(from_plugin_dir, Path::new(&bundle_path))
            .map_err(|e| plugin_error(WHERE, "app.plugin.mvdir.app_error", 500, e))?;

        let mut manifest = manifest;
        if manifest.has_webapp() {
            manifest = environment
                .unpack_webapp_bundle(&manifest.id)
                .map_err(|e| plugin_error(WHERE, "app.plugin.webapp_bundle.app_error", 500, e))?;
        }

        // Activate it when the configuration already enables it.
        if config.plugin_states.get(&manifest.id).copied() == Some(true) {
            if manifest.id == crate::plugins::PLUGIN_ID_APPS && !config.feature_flag_apps_enabled {
                return Ok(manifest);
            }
            match environment.activate(&manifest.id).await {
                Ok(Some(updated)) => manifest = updated,
                Ok(None) => {
                    return Err(AppError::boxed(
                        WHERE,
                        "app.plugin.restart.app_error",
                        None,
                        "failed to activate plugin: plugin already active",
                        500,
                    ));
                }
                Err(e) => return Err(plugin_error(WHERE, "app.plugin.restart.app_error", 500, e)),
            }
        }
        Ok(manifest)
    }

    /// Port of `removePluginLocally` (plugin_install.go:558): deactivate, forget, and delete the
    /// unpacked bundle.
    pub async fn remove_plugin_locally(&self, id: &str) -> Result<(), Box<AppError>> {
        const WHERE: &str = "removePlugin";
        let Some(environment) = self.plugins_environment() else {
            return Err(AppError::boxed(
                WHERE,
                "app.plugin.disabled.app_error",
                None,
                "",
                501,
            ));
        };
        let plugins = environment
            .available()
            .map_err(|e| plugin_error(WHERE, "app.plugin.deactivate.app_error", 400, e))?;
        let Some(unpacked) = plugins
            .iter()
            .find(|p| p.manifest.as_ref().is_some_and(|m| m.id == id))
            .map(|p| go_path::dir(&p.manifest_path))
        else {
            return Err(AppError::boxed(
                WHERE,
                "app.plugin.not_installed.app_error",
                None,
                "",
                404,
            ));
        };
        environment.deactivate(id).await;
        environment.remove_plugin(id);
        if let Err(e) = std::fs::remove_dir_all(&unpacked)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(plugin_error(WHERE, "app.plugin.remove.app_error", 500, e));
        }
        Ok(())
    }

    /// Port of `Channels.syncPlugins` (app/plugin.go:265): remove every locally installed plugin,
    /// then install every bundle the file store holds under `plugins/`.
    pub async fn sync_plugins(&self) -> Result<(), Box<AppError>> {
        tracing::info!("Syncing plugins from the file store");
        let Some(environment) = self.plugins_environment() else {
            return Err(AppError::boxed(
                "SyncPlugins",
                "app.plugin.disabled.app_error",
                None,
                "",
                501,
            ));
        };
        let available = environment.available().map_err(|e| {
            plugin_error(
                "SyncPlugins",
                "app.plugin.sync.read_local_folder.app_error",
                500,
                e,
            )
        })?;
        let removals = available.into_iter().filter_map(|p| p.manifest).map(|m| async move {
            tracing::info!(plugin_id = %m.id, "Removing local installation of managed plugin before sync");
            if let Err(err) = self.remove_plugin_locally(&m.id).await {
                tracing::error!(plugin_id = %m.id, error = %err, "Failed to remove local installation of managed plugin before sync");
            }
        });
        crate::plugins::join_all(removals.collect()).await;

        let paths = self
            .file_backend()
            .list_directory(FILE_STORE_PLUGIN_FOLDER)
            .await
            .map_err(|e| {
                plugin_error(
                    "getPluginsFromDir",
                    "app.plugin.sync.list_filestore.app_error",
                    500,
                    e,
                )
            })?;
        let found = plugins_from_file_paths(&paths);
        if found.is_empty() {
            tracing::info!("No plugins to sync from the file store");
            return Ok(());
        }
        let require_signature = self.config().plugin_require_signature;
        let installs = found.into_values().map(|plugin| async move {
            let bundle = match self.file_backend().read_file(&plugin.bundle_path).await {
                Ok(bundle) => bundle,
                Err(err) => {
                    tracing::error!(plugin_id = %plugin.plugin_id, error = %err, "Failed to open plugin bundle from file store.");
                    return;
                }
            };
            if require_signature {
                tracing::error!(
                    plugin_id = %plugin.plugin_id,
                    "Failed to validate plugin signature: signature verification is not ported (D-811)"
                );
                return;
            }
            tracing::info!(plugin_id = %plugin.plugin_id, "Syncing plugin from file store");
            if let Err(err) = self
                .install_plugin_locally(&bundle, InstallStrategy::Always)
                .await
                && err.id != "app.plugin.skip_installation.app_error"
            {
                tracing::error!(plugin_id = %plugin.plugin_id, error = %err, "Failed to sync plugin from file store");
            }
        });
        crate::plugins::join_all(installs.collect()).await;
        Ok(())
    }
}

/// `os.MkdirTemp("", "plugintmp")`, removed on drop as Go's `defer os.RemoveAll` does.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> std::io::Result<Self> {
        let base = std::env::temp_dir();
        for _ in 0..10_000 {
            let candidate = base.join(format!("plugintmp{}", mm_model::utils::new_id()));
            match std::fs::create_dir(&candidate) {
                Ok(()) => return Ok(Self(candidate)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_paths_are_gos() {
        assert_eq!(bundle_store_path("com.x"), "plugins/com.x.tar.gz");
        assert_eq!(signature_store_path("com.x"), "plugins/com.x.tar.gz.sig");
    }

    /// Bundles by id, signatures attached to theirs, and a stray signature dropped.
    #[test]
    fn file_paths_pair_bundles_with_signatures() {
        let paths: Vec<String> = [
            "plugins/a.tar.gz",
            "plugins/a.tar.gz.sig",
            "plugins/b.tar.gz",
            "plugins/stray.tar.gz.sig",
            "plugins/readme.txt",
        ]
        .map(str::to_owned)
        .to_vec();
        let found = plugins_from_file_paths(&paths);
        assert_eq!(found.len(), 2);
        assert_eq!(found["a"].signature_path, "plugins/a.tar.gz.sig");
        assert_eq!(found["b"].signature_path, "");
        assert_eq!(found["b"].bundle_path, "plugins/b.tar.gz");
    }

    #[test]
    fn an_empty_destination_is_refused() {
        assert!(matches!(
            extract_tar_gz(b"", ""),
            Err(ExtractError::NoDestination)
        ));
    }
}

/// `extractTarGz` against Go's own, over `reference/dump/plugintar`'s corpus.
///
/// The oracle compiles `server/channels/app/extract_plugin_tar.go` unchanged but for its package
/// clause, so what it records is the pinned function's behaviour. Both run in this process's
/// umask, so the modes compare.
#[cfg(test)]
mod go_parity {
    use super::*;
    use std::process::Command;

    fn dump() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../reference/dump")
    }

    /// Build the oracle with the pinned source copied in beside it.
    fn plugintar() -> PathBuf {
        let source = std::fs::read_to_string(
            dump().join("../mattermost/server/channels/app/extract_plugin_tar.go"),
        )
        .expect("the pinned extract_plugin_tar.go");
        let copied = source.replacen("package app\n", "package main\n", 1);
        std::fs::write(dump().join("plugintar/zz_extract_plugin_tar.go"), copied).unwrap();
        let out = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join("plugintar");
        let status = Command::new("go")
            .args(["build", "-o"])
            .arg(&out)
            .arg("./plugintar")
            .current_dir(dump())
            .status()
            .expect("the oracle needs a Go toolchain on PATH");
        assert!(status.success(), "building reference/dump/plugintar failed");
        out
    }

    fn walk(root: &Path) -> Vec<serde_json::Value> {
        use std::os::unix::fs::PermissionsExt as _;
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                let meta = std::fs::symlink_metadata(&path).unwrap();
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                let mode = format!("{:04o}", meta.permissions().mode() & 0o777);
                let value = if meta.is_dir() {
                    stack.push(path);
                    serde_json::json!({"path": rel, "type": "dir", "mode": mode})
                } else if meta.file_type().is_symlink() {
                    serde_json::json!({"path": rel, "type": "symlink", "mode": mode})
                } else {
                    let body = String::from_utf8_lossy(&std::fs::read(&path).unwrap_or_default())
                        .into_owned();
                    let mut v = serde_json::json!({"path": rel, "type": "file", "mode": mode});
                    if !body.is_empty() {
                        v["content"] = serde_json::Value::String(body);
                    }
                    v
                };
                out.push(value);
            }
        }
        out.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
        out
    }

    #[test]
    fn extraction_matches_go_for_every_corpus_bundle() {
        let oracle = plugintar();
        // A clean path: the check is a string prefix against the joined (so cleaned) name, and Go's
        // destination is always a fresh `MkdirTemp`, which is clean.
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp");
        std::fs::create_dir_all(&target).unwrap();
        let scratch = target.canonicalize().unwrap().join("plugintar-parity");
        let _ = std::fs::remove_dir_all(&scratch);
        let (corpus, go_work, rust_work) = (
            scratch.join("corpus"),
            scratch.join("go"),
            scratch.join("rust"),
        );
        for d in [&corpus, &go_work, &rust_work] {
            std::fs::create_dir_all(d).unwrap();
        }
        let status = Command::new(&oracle)
            .arg("corpus")
            .arg(&corpus)
            .status()
            .unwrap();
        assert!(status.success());
        let out = Command::new(&oracle)
            .arg("extract")
            .arg(&corpus)
            .arg(&go_work)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let go: BTreeMap<String, serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
        assert!(go.len() >= 20, "the corpus: {}", go.len());

        let mut failures = Vec::new();
        for (name, want) in &go {
            let root = rust_work.join(name);
            let dst = root.join("out");
            // `os.MkdirAll(dst, 0o755)`, as the oracle makes it.
            std::os::unix::fs::DirBuilderExt::mode(
                std::fs::DirBuilder::new().recursive(true),
                0o755,
            )
            .create(&dst)
            .unwrap();
            let bundle = std::fs::read(corpus.join(format!("{name}.tar.gz"))).unwrap();
            let result = extract_tar_gz(&bundle, &dst.to_string_lossy());
            let got = serde_json::json!({
                "failed": result.is_err(),
                "entries": walk(&root),
            });
            let expected = serde_json::json!({
                "failed": want["failed"],
                "entries": want["entries"],
            });
            if got != expected {
                failures.push(format!(
                    "{name}: Go {} ({})\n  Rust {} ({:?})",
                    expected,
                    want["error"],
                    got,
                    result.err().map(|e| e.to_string())
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
