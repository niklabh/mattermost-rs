//! The plugin environment: which plugins are installed, which are running, and the supervisor of
//! each running one (public/plugin/environment.go and supervisor.go, with model/manifest.go's
//! `FindManifest` and model/bundle_info.go's `BundleInfoForPath`).
//!
//! Go's behaviour is kept where it is surprising, because a status a System Console shows or an
//! error a plugin developer reads comes straight from here:
//!
//! - **A webapp bundle is unpacked from `<plugin dir>/<id>`, not from the bundle's own
//!   directory.** A bundle whose directory is not named after its (lowercased) id cannot activate
//!   a webapp: the copy fails with `stat …: no such file or directory`.
//! - **`Activate` replaces the registration before it checks anything**, so a failed activation
//!   of a plugin that was registered before drops its old supervisor.
//! - **A plugin that failed with an error keeps it through `Deactivate`**: the state goes back to
//!   not running, the error stays. `Shutdown` forgets both.
//! - **`Deactivate` leaves the supervisor registered**; only `IsActive` stops lookups reaching it.
//! - Duplicates and missing bundles are refused **without** registering, so their status stays
//!   not running with no error.
//!
//! Errors carry Go's text, since a status shows it. I/O errors use Go's `op path: errno` form
//! ([`GoIoError`]); the errno wording covers the common cases and otherwise falls back to Rust's.
//!
//! Not ported yet: `Reattach` (the local-mode route that needs it), the health-check job,
//! prepackaged plugins, and the per-plugin database connections Go's `AppDriver` tracks
//! (`ConnWithPluginID`, `ShutdownConns`), which belong to the app's driver.
//!
//! Divergences: Go decodes `plugin.json` with case-insensitive keys ([D-040]) and YAML with
//! goccy/go-yaml; here JSON keys match exactly and YAML goes through `serde_yaml_ng`. Neither
//! changes a manifest written the way the documentation shows. Go's metrics layer
//! (`hooksTimerLayer`) is not here.

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use goplugin::{Client, ClientConfig, PluginCommand};
use mm_model::bundle_info::BundleInfo;
use mm_model::manifest::Manifest;
use mm_model::plugin_status::{
    PLUGIN_STATE_FAILED_TO_START, PLUGIN_STATE_NOT_RUNNING, PLUGIN_STATE_RUNNING, PluginStatus,
    PluginStatuses,
};

use crate::error::decodable_error;
use crate::rpc::{Driver, HooksClient, PluginApi, PluginApiHttp, PluginApiStreams, handshake};
use crate::wire::plugin::Z_OnDeactivateArgs;

/// Go's `model.CurrentVersion`, which `min_server_version` is checked against.
pub const CURRENT_VERSION: &str = mm_model::utils::CURRENT_VERSION;

/// How long `OnDeactivate` may take before the plugin is torn down regardless.
const DEACTIVATE_TIMEOUT: Duration = Duration::from_secs(10);

/// supervisor.go: `StartTimeout: time.Second * 3`.
const START_TIMEOUT: Duration = Duration::from_secs(3);

/// An I/O error as Go's `*fs.PathError` prints it: `open /x: no such file or directory`.
#[derive(Debug, thiserror::Error)]
#[error("{op} {path}: {}", errno(.source))]
pub struct GoIoError {
    pub op: &'static str,
    pub path: String,
    #[source]
    pub source: io::Error,
}

impl GoIoError {
    fn new(op: &'static str, path: &Path, source: io::Error) -> Self {
        Self {
            op,
            path: path.to_string_lossy().into_owned(),
            source,
        }
    }
}

/// Go's `syscall.Errno` text for the errors a plugin directory produces.
fn errno(e: &io::Error) -> String {
    let known = match e.raw_os_error() {
        Some(2) => "no such file or directory",
        Some(13) => "permission denied",
        Some(17) => "file exists",
        Some(20) => "not a directory",
        Some(21) => "is a directory",
        Some(39) => "directory not empty",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_owned();
    }
    let text = e.to_string();
    let text = text.split(" (os error").next().unwrap_or_default();
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_lowercase().chain(chars).collect()
    })
}

/// Every way the environment fails, each displayed with Go's text.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EnvError {
    #[error(transparent)]
    Io(#[from] GoIoError),
    /// environment.go's `ErrNotFound`.
    #[error("Item not found")]
    ItemNotFound,
    #[error("plugin not found: {0}")]
    NotFound(String),
    #[error("multiple plugins found: {0}")]
    Multiple(String),
    #[error("unable to get plugin statuses: {0}")]
    Statuses(Box<EnvError>),
    #[error("{source}: {id}")]
    MinServerVersion {
        id: String,
        source: mm_model::manifest::ManifestError,
    },
    #[error("plugin requires Mattermost {min}: {id}")]
    RequiresVersion { min: String, id: String },
    #[error("unable to generate webapp bundle: {id}: {source}")]
    WebappBundle { id: String, source: Box<EnvError> },
    #[error("unable to start plugin: {id}: {source}")]
    StartPlugin { id: String, source: Box<EnvError> },
    #[error("unable to start plugin: must at least have a web app or server component")]
    NoComponent,
    #[error("failed to apply option: {0}")]
    ApplyOption(Box<EnvError>),
    #[error("backend executable not found for environment: {os}/{arch}")]
    NoExecutable {
        os: &'static str,
        arch: &'static str,
    },
    #[error("invalid backend executable: {0}")]
    InvalidExecutable(String),
    #[error("unable to generate plugin checksum: {0}")]
    Checksum(Box<EnvError>),
    #[error("Unable to get available plugins")]
    Unavailable,
    #[error("invalid webapp bundle path")]
    InvalidBundlePath,
    #[error("unable to remove old webapp bundle directory: {dest}: {source}")]
    RemoveOldBundle { dest: String, source: GoIoError },
    #[error("unable to copy webapp bundle directory: {id}: {source}")]
    CopyBundle { id: String, source: Box<EnvError> },
    #[error("unable to read webapp bundle: {id}: {source}")]
    ReadBundle { id: String, source: GoIoError },
    #[error("unable to rename webapp bundle: {id}: {source}")]
    RenameBundle { id: String, source: GoIoError },
    /// utils.CopyDir.
    #[error("source must be a directory")]
    SourceNotDirectory,
    #[error("destination already exists")]
    DestinationExists,
    #[error(transparent)]
    Launch(#[from] goplugin::ClientError),
    #[error(transparent)]
    Rpc(#[from] go_netrpc::Error),
    /// What the plugin's `OnActivate` answered, as Go prints it.
    #[error("{0}")]
    Activate(String),
}

// ---------------------------------------------------------------------------------------------
// Go's path helpers
// ---------------------------------------------------------------------------------------------

use mm_model::go_path::{base as go_base, clean as go_clean, dir as go_dir, join as go_join};

fn lossy(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Go's `runtime.GOOS` and `runtime.GOARCH` for this build, which name a manifest's executables.
pub fn go_platform() -> (&'static str, &'static str) {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "powerpc64" => "ppc64",
        "loongarch64" => "loong64",
        other => other,
    };
    (os, arch)
}

// ---------------------------------------------------------------------------------------------
// Manifests and bundles
// ---------------------------------------------------------------------------------------------

/// Why a bundle's manifest could not be read (model/manifest.go, `FindManifest`).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ManifestReadError {
    #[error(transparent)]
    Io(#[from] GoIoError),
    #[error("{0}")]
    Yaml(#[from] serde_yaml_ng::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
}

/// model/manifest.go, `FindManifest`: `plugin.yml`, then `plugin.yaml`, then `plugin.json`,
/// with the id lowercased. Answers the manifest, the path it was read from, and the error.
///
/// The path is empty when nothing was found, and also when a YAML file exists but cannot be
/// opened, which is Go's quirk. JSON is read as Go's `json.Decoder` reads it: the first value,
/// with anything after it ignored.
pub fn find_manifest(dir: &Path) -> (Option<Manifest>, String, Option<ManifestReadError>) {
    for name in ["plugin.yml", "plugin.yaml"] {
        let path = dir.join(name);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                return (
                    None,
                    String::new(),
                    Some(GoIoError::new("open", &path, e).into()),
                );
            }
        };
        return match serde_yaml_ng::from_slice::<Manifest>(&bytes) {
            Ok(mut manifest) => {
                manifest.id = manifest.id.to_lowercase();
                (Some(manifest), lossy(&path), None)
            }
            Err(e) => (None, lossy(&path), Some(e.into())),
        };
    }

    let path = dir.join("plugin.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) => {
            let shown = if e.kind() == io::ErrorKind::NotFound {
                String::new()
            } else {
                lossy(&path)
            };
            return (None, shown, Some(GoIoError::new("open", &path, e).into()));
        }
    };
    let first = serde_json::Deserializer::from_slice(&bytes)
        .into_iter::<Manifest>()
        .next();
    match first {
        Some(Ok(mut manifest)) => {
            manifest.id = manifest.id.to_lowercase();
            (Some(manifest), lossy(&path), None)
        }
        Some(Err(e)) => (None, lossy(&path), Some(e.into())),
        None => (
            None,
            lossy(&path),
            Some(serde_json::Error::io(io::Error::from(io::ErrorKind::UnexpectedEof)).into()),
        ),
    }
}

/// model/bundle_info.go, `BundleInfoForPath`.
pub fn bundle_info_for_path(path: &Path) -> BundleInfo {
    let (manifest, manifest_path, error) = find_manifest(path);
    BundleInfo {
        path: lossy(path),
        manifest,
        manifest_path,
        manifest_error: error.map(|e| e.to_string()),
    }
}

/// environment.go, `scanSearchPath`: every directory directly under `path` that has a readable
/// manifest, by name, skipping names that begin with a dot. A symlink is not a directory here,
/// as Go's `DirEntry.IsDir` does not follow it.
pub fn scan_search_path(path: &Path) -> Result<Vec<BundleInfo>, EnvError> {
    let read = std::fs::read_dir(path).map_err(|e| GoIoError::new("open", path, e))?;
    let mut entries = Vec::new();
    for entry in read {
        let entry = entry.map_err(|e| GoIoError::new("readdirent", path, e))?;
        entries.push(entry);
    }
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let mut infos = Vec::new();
    for entry in entries {
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        let name = entry.file_name();
        if !is_dir || name.to_string_lossy().starts_with('.') {
            continue;
        }
        let dir = PathBuf::from(go_join(&[&lossy(path), &name.to_string_lossy()]));
        let info = bundle_info_for_path(&dir);
        if info.manifest.is_some() {
            infos.push(info);
        }
    }
    Ok(infos)
}

/// public/utils/file.go, `CopyDir`: `src` must be a directory and `dst` must not exist.
/// Permissions are kept and symlinks skipped.
pub fn copy_dir(src: &Path, dst: &Path) -> Result<(), EnvError> {
    let stat = std::fs::metadata(src).map_err(|e| GoIoError::new("stat", src, e))?;
    if !stat.is_dir() {
        return Err(EnvError::SourceNotDirectory);
    }
    match std::fs::metadata(dst) {
        Ok(_) => return Err(EnvError::DestinationExists),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(GoIoError::new("stat", dst, e).into()),
    }
    std::fs::create_dir_all(dst).map_err(|e| GoIoError::new("mkdir", dst, e))?;
    std::fs::set_permissions(dst, stat.permissions())
        .map_err(|e| GoIoError::new("chmod", dst, e))?;

    let mut items: Vec<std::fs::DirEntry> = std::fs::read_dir(src)
        .map_err(|e| GoIoError::new("open", src, e))?
        .filter_map(Result::ok)
        .collect();
    items.sort_by_key(std::fs::DirEntry::file_name);
    for item in items {
        let from = src.join(item.file_name());
        let to = dst.join(item.file_name());
        let Ok(kind) = item.file_type() else {
            continue;
        };
        if kind.is_dir() {
            copy_dir(&from, &to)?;
        } else if !kind.is_symlink() {
            std::fs::copy(&from, &to).map_err(|e| GoIoError::new("open", &from, e))?;
        }
    }
    Ok(())
}

/// 64-bit FNV-1a, which Go hashes a webapp bundle with.
fn fnv64a(data: &[u8]) -> [u8; 8] {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash.to_be_bytes()
}

// ---------------------------------------------------------------------------------------------
// The supervisor
// ---------------------------------------------------------------------------------------------

/// One running plugin process and the hooks client on it (supervisor.go).
pub struct Supervisor {
    client: Client,
    hooks: Arc<HooksClient>,
}

impl Supervisor {
    /// supervisor.go, `newSupervisor` with `WithExecutableFromManifest`: launch the executable the
    /// manifest names for this platform, checked against its SHA-256, dispense the hooks and ask
    /// which are implemented.
    async fn start(bundle: &BundleInfo, manifest: &Manifest) -> Result<Self, EnvError> {
        let command =
            executable_command(bundle, manifest).map_err(|e| EnvError::ApplyOption(Box::new(e)))?;
        let mut config = ClientConfig::new(handshake());
        config.checksum = Some(command.1);
        config.cmd = Some(PluginCommand::new(command.0));
        config.start_timeout = START_TIMEOUT;
        config.name = manifest.id.clone();
        let client = Client::start(config).await?;
        let dispensed = match client.dispense("hooks").await {
            Ok(d) => d,
            Err(e) => {
                client.kill().await;
                return Err(e.into());
            }
        };
        let hooks = Arc::new(HooksClient::new(dispensed));
        if let Err(e) = hooks.implemented().await {
            client.kill().await;
            return Err(e.into());
        }
        Ok(Self { client, hooks })
    }

    pub fn hooks(&self) -> &Arc<HooksClient> {
        &self.hooks
    }

    /// supervisor.go, `Shutdown`.
    pub async fn shutdown(&self) {
        self.client.kill().await;
    }

    /// supervisor.go, `Ping`.
    pub async fn ping(&self) -> Result<(), goplugin::ClientError> {
        self.client.ping().await
    }
}

/// supervisor.go, `WithExecutableFromManifest`: the executable's path and checksum.
fn executable_command(
    bundle: &BundleInfo,
    manifest: &Manifest,
) -> Result<(PathBuf, Vec<u8>), EnvError> {
    let (os, arch) = go_platform();
    let executable = manifest.get_executable_for_runtime(os, arch);
    if executable.is_empty() {
        return Err(EnvError::NoExecutable { os, arch });
    }
    let executable = go_clean(&go_join(&[".", executable]));
    if executable.starts_with("..") {
        return Err(EnvError::InvalidExecutable(executable));
    }
    let path = PathBuf::from(go_join(&[&bundle.path, &executable]));
    let checksum = goplugin::client::sha256_file(&path)
        .map_err(|e| EnvError::Checksum(Box::new(GoIoError::new("open", &path, e).into())))?;
    Ok((path, checksum))
}

// ---------------------------------------------------------------------------------------------
// The environment
// ---------------------------------------------------------------------------------------------

/// environment.go, `registeredPlugin`.
#[derive(Clone)]
struct Registered {
    bundle: BundleInfo,
    state: i64,
    error: String,
    supervisor: Option<Arc<Supervisor>>,
}

/// Makes the API each plugin is served (environment.go, `apiImplCreatorFunc`).
pub type ApiFactory<A> = Box<dyn Fn(&Manifest) -> Arc<A> + Send + Sync>;

/// environment.go, `Environment`.
pub struct Environment<A, D> {
    registered: Mutex<BTreeMap<String, Registered>>,
    new_api: ApiFactory<A>,
    driver: Arc<D>,
    plugin_dir: PathBuf,
    webapp_plugin_dir: PathBuf,
}

impl<A, D> Environment<A, D>
where
    A: PluginApi + PluginApiStreams + PluginApiHttp,
    D: Driver,
{
    pub fn new(
        new_api: ApiFactory<A>,
        driver: Arc<D>,
        plugin_dir: PathBuf,
        webapp_plugin_dir: PathBuf,
    ) -> Self {
        Self {
            registered: Mutex::new(BTreeMap::new()),
            new_api,
            driver,
            plugin_dir,
            webapp_plugin_dir,
        }
    }

    fn map(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Registered>> {
        self.registered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn get(&self, id: &str) -> Option<Registered> {
        self.map().get(id).cloned()
    }

    fn update(&self, id: &str, f: impl FnOnce(&mut Registered)) {
        if let Some(r) = self.map().get_mut(id) {
            f(r);
        }
    }

    /// Every bundle in the plugin directory with a readable manifest.
    pub fn available(&self) -> Result<Vec<BundleInfo>, EnvError> {
        scan_search_path(&self.plugin_dir)
    }

    /// Every running plugin.
    pub fn active(&self) -> Vec<BundleInfo> {
        self.map()
            .values()
            .filter(|r| r.state == PLUGIN_STATE_RUNNING)
            .map(|r| r.bundle.clone())
            .collect()
    }

    pub fn is_active(&self, id: &str) -> bool {
        self.get_plugin_state(id) == PLUGIN_STATE_RUNNING
    }

    /// Record an error against a registered plugin; an unregistered one is left alone.
    pub fn set_plugin_error(&self, id: &str, error: &str) {
        self.update(id, |r| r.error = error.to_owned());
    }

    fn plugin_error(&self, id: &str) -> String {
        self.get(id).map(|r| r.error).unwrap_or_default()
    }

    /// The state of a plugin; one never registered is not running.
    pub fn get_plugin_state(&self, id: &str) -> i64 {
        self.get(id).map_or(PLUGIN_STATE_NOT_RUNNING, |r| r.state)
    }

    fn set_plugin_state(&self, id: &str, state: i64) {
        self.update(id, |r| r.state = state);
    }

    /// Where a running plugin's public files are.
    pub fn public_files_path(&self, id: &str) -> Result<PathBuf, EnvError> {
        if !self.is_active(id) {
            return Err(EnvError::NotFound(id.to_owned()));
        }
        Ok(PathBuf::from(go_join(&[
            &lossy(&self.plugin_dir),
            id,
            "public",
        ])))
    }

    /// The status of every available plugin, in directory order.
    pub fn statuses(&self) -> Result<PluginStatuses, EnvError> {
        let plugins = self
            .available()
            .map_err(|e| EnvError::Statuses(Box::new(e)))?;
        let mut statuses = Vec::with_capacity(plugins.len());
        for plugin in plugins {
            let Some(manifest) = plugin.manifest else {
                continue;
            };
            statuses.push(PluginStatus {
                plugin_id: manifest.id.clone(),
                cluster_id: String::new(),
                plugin_path: go_dir(&plugin.manifest_path),
                state: self.get_plugin_state(&manifest.id),
                error: self.plugin_error(&manifest.id),
                name: manifest.name,
                description: manifest.description,
                version: manifest.version,
            });
        }
        Ok(PluginStatuses(statuses))
    }

    /// The manifest of an available plugin, or Go's `ErrNotFound`.
    pub fn get_manifest(&self, id: &str) -> Result<Manifest, EnvError> {
        let plugins = self
            .available()
            .map_err(|e| EnvError::Statuses(Box::new(e)))?;
        plugins
            .into_iter()
            .filter_map(|p| p.manifest)
            .find(|m| m.id == id)
            .ok_or(EnvError::ItemNotFound)
    }

    /// The one available bundle with this id.
    fn unique_bundle(&self, id: &str) -> Result<BundleInfo, EnvError> {
        let mut found: Option<BundleInfo> = None;
        for p in self.available()? {
            if p.manifest.as_ref().is_some_and(|m| m.id == id) {
                if found.is_some() {
                    return Err(EnvError::Multiple(id.to_owned()));
                }
                found = Some(p);
            }
        }
        found.ok_or_else(|| EnvError::NotFound(id.to_owned()))
    }

    /// Start a plugin: `Some(manifest)` when it was activated, `None` when it was already running.
    /// Whatever happens, the plugin's error is set to the outcome's.
    pub async fn activate(&self, id: &str) -> Result<Option<Manifest>, EnvError> {
        let result = self.try_activate(id).await;
        let error = result.as_ref().err().map(ToString::to_string);
        self.set_plugin_error(id, error.as_deref().unwrap_or(""));
        result
    }

    async fn try_activate(&self, id: &str) -> Result<Option<Manifest>, EnvError> {
        if self.is_active(id) {
            return Ok(None);
        }
        let bundle = self.unique_bundle(id)?;
        self.map().insert(
            id.to_owned(),
            Registered {
                bundle: bundle.clone(),
                state: PLUGIN_STATE_NOT_RUNNING,
                error: String::new(),
                supervisor: None,
            },
        );
        let result = self.activate_registered(id, bundle).await;
        self.set_plugin_state(
            id,
            if result.is_ok() {
                PLUGIN_STATE_RUNNING
            } else {
                PLUGIN_STATE_FAILED_TO_START
            },
        );
        result.map(Some)
    }

    async fn activate_registered(
        &self,
        id: &str,
        mut bundle: BundleInfo,
    ) -> Result<Manifest, EnvError> {
        let Some(mut manifest) = bundle.manifest.take() else {
            return Err(EnvError::NotFound(id.to_owned()));
        };
        check_min_server_version(&manifest)?;

        let mut component = false;
        if manifest.has_webapp() {
            let unpacked = self
                .unpack_webapp_bundle(id)
                .map_err(|e| EnvError::WebappBundle {
                    id: id.to_owned(),
                    source: Box::new(e),
                })?;
            let hash = unpacked.webapp.map(|w| w.bundle_hash).unwrap_or_default();
            if let Some(webapp) = manifest.webapp.as_mut() {
                webapp.bundle_hash = hash.clone();
            }
            // Go shares the manifest by pointer, so the registered one carries the hash too.
            self.update(id, |r| {
                if let Some(webapp) = r.bundle.manifest.as_mut().and_then(|m| m.webapp.as_mut()) {
                    webapp.bundle_hash = hash;
                }
            });
            component = true;
        }

        if manifest.has_server() {
            bundle.manifest = Some(manifest.clone());
            self.start_plugin_server(&bundle, &manifest).await?;
            component = true;
        }

        if !component {
            return Err(EnvError::NoComponent);
        }
        tracing::debug!(plugin_id = %manifest.id, version = %manifest.version, "Plugin activated");
        Ok(manifest)
    }

    /// environment.go, `startPluginServer`. The state is running before `OnActivate`, so a plugin
    /// that reconfigures itself from there does not start twice.
    async fn start_plugin_server(
        &self,
        bundle: &BundleInfo,
        manifest: &Manifest,
    ) -> Result<(), EnvError> {
        let supervisor =
            Supervisor::start(bundle, manifest)
                .await
                .map_err(|e| EnvError::StartPlugin {
                    id: manifest.id.clone(),
                    source: Box::new(e),
                })?;
        self.set_plugin_state(&manifest.id, PLUGIN_STATE_RUNNING);

        let api = (self.new_api)(manifest);
        let returns = supervisor.hooks.on_activate(&api, &self.driver).await;
        if let Some(error) = decodable_error(returns.a.as_ref()) {
            supervisor.shutdown().await;
            return Err(EnvError::Activate(error.go_error()));
        }
        let supervisor = Arc::new(supervisor);
        self.update(&manifest.id, |r| r.supervisor = Some(supervisor));
        Ok(())
    }

    /// environment.go, `UnpackWebappBundle`: copy the bundle's directory into the webapp
    /// directory and rename the bundle after its FNV-1a hash.
    pub fn unpack_webapp_bundle(&self, id: &str) -> Result<Manifest, EnvError> {
        let bundle = self.unique_bundle(id).map_err(|e| match e {
            EnvError::Multiple(_) | EnvError::NotFound(_) => e,
            _ => EnvError::Unavailable,
        })?;
        let Some(mut manifest) = bundle.manifest else {
            return Err(EnvError::NotFound(id.to_owned()));
        };
        let bundle_path = go_clean(
            manifest
                .webapp
                .as_ref()
                .map_or("", |w| w.bundle_path.as_str()),
        );
        if bundle_path.is_empty() || bundle_path.starts_with('.') {
            return Err(EnvError::InvalidBundlePath);
        }
        let bundle_path = go_join(&[&lossy(&self.plugin_dir), id, &bundle_path]);
        let destination = go_join(&[&lossy(&self.webapp_plugin_dir), id]);

        if let Err(e) = std::fs::remove_dir_all(&destination)
            && e.kind() != io::ErrorKind::NotFound
        {
            return Err(EnvError::RemoveOldBundle {
                dest: destination.clone(),
                source: GoIoError::new("unlinkat", Path::new(&destination), e),
            });
        }
        copy_dir(Path::new(&go_dir(&bundle_path)), Path::new(&destination)).map_err(|e| {
            EnvError::CopyBundle {
                id: id.to_owned(),
                source: Box::new(e),
            }
        })?;

        let source = go_join(&[&destination, &go_base(&bundle_path)]);
        let contents = std::fs::read(&source).map_err(|e| EnvError::ReadBundle {
            id: id.to_owned(),
            source: GoIoError::new("open", Path::new(&source), e),
        })?;
        let hash = fnv64a(&contents);
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        let renamed = go_join(&[&destination, &format!("{id}_{hex}_bundle.js")]);
        std::fs::rename(&source, &renamed).map_err(|e| EnvError::RenameBundle {
            id: id.to_owned(),
            source: GoIoError {
                op: "rename",
                path: format!("{source} {renamed}"),
                source: e,
            },
        })?;
        if let Some(webapp) = manifest.webapp.as_mut() {
            webapp.bundle_hash = hash.to_vec();
        }
        Ok(manifest)
    }

    /// Forget a plugin's registration.
    pub fn remove_plugin(&self, id: &str) {
        self.map().remove(id);
    }

    /// environment.go, `deactivateAndTeardown`: `OnDeactivate` with ten seconds to answer, then
    /// the state back to not running and the process ended. A plugin that is not running only
    /// has its state reconciled. Answers whether there was anything to tear down.
    async fn deactivate_and_teardown(&self, registered: Registered) -> bool {
        let id = registered
            .bundle
            .manifest
            .as_ref()
            .map(|m| m.id.clone())
            .unwrap_or_default();
        let supervisor = match registered.supervisor {
            Some(s) if self.is_active(&id) => s,
            _ => {
                self.set_plugin_state(&id, PLUGIN_STATE_NOT_RUNNING);
                return false;
            }
        };
        let deactivate = supervisor.hooks.on_deactivate(Z_OnDeactivateArgs {});
        match tokio::time::timeout(DEACTIVATE_TIMEOUT, deactivate).await {
            Ok(returns) => {
                if let Some(error) = decodable_error(returns.a.as_ref()) {
                    tracing::error!(plugin_id = %id, error = %error.go_error(), "Plugin OnDeactivate() error");
                }
            }
            Err(_) => tracing::warn!(
                plugin_id = %id,
                "Plugin OnDeactivate() failed to complete in 10 seconds"
            ),
        }
        self.set_plugin_state(&id, PLUGIN_STATE_NOT_RUNNING);
        supervisor.shutdown().await;
        true
    }

    /// Stop a plugin; whether it was running.
    pub async fn deactivate(&self, id: &str) -> bool {
        match self.get(id) {
            Some(registered) => self.deactivate_and_teardown(registered).await,
            None => false,
        }
    }

    pub async fn restart_plugin(&self, id: &str) -> Result<(), EnvError> {
        self.deactivate(id).await;
        self.activate(id).await.map(drop)
    }

    /// Deactivate every plugin at once, then forget them all.
    pub async fn shutdown(&self) {
        let registered: Vec<Registered> = self.map().values().cloned().collect();
        let teardowns = registered
            .into_iter()
            .map(|r| self.deactivate_and_teardown(r));
        futures_join_all(teardowns).await;
        self.map().clear();
    }

    /// The hooks of a running plugin.
    pub fn hooks_for_plugin(&self, id: &str) -> Result<Arc<HooksClient>, EnvError> {
        match self.get(id) {
            Some(Registered {
                supervisor: Some(s),
                ..
            }) if self.is_active(id) => Ok(Arc::clone(&s.hooks)),
            _ => Err(EnvError::NotFound(id.to_owned())),
        }
    }

    /// The running plugins that implement `hook_id`, with their manifests.
    fn implementing(&self, hook_id: usize) -> Vec<(Arc<HooksClient>, Manifest)> {
        self.map()
            .values()
            .filter(|r| r.state == PLUGIN_STATE_RUNNING)
            .filter_map(|r| {
                let s = r.supervisor.as_ref()?;
                let m = r.bundle.manifest.clone()?;
                s.hooks
                    .implements(hook_id)
                    .then(|| (Arc::clone(&s.hooks), m))
            })
            .collect()
    }

    /// Whether any running plugin implements `hook_id`.
    pub fn has_plugin_implementing(&self, hook_id: usize) -> bool {
        !self.implementing(hook_id).is_empty()
    }

    /// environment.go, `RunMultiPluginHook`: call `f` for each running plugin implementing
    /// `hook_id` until it answers `false`. The order among plugins is unspecified, as in Go.
    pub async fn run_multi_plugin_hook<F, Fut>(&self, hook_id: usize, mut f: F)
    where
        F: FnMut(Arc<HooksClient>, Manifest) -> Fut,
        Fut: Future<Output = bool>,
    {
        for (hooks, manifest) in self.implementing(hook_id) {
            if !f(hooks, manifest).await {
                return;
            }
        }
    }
}

/// environment.go, `checkMinServerVersion`.
fn check_min_server_version(manifest: &Manifest) -> Result<(), EnvError> {
    if manifest.min_server_version.is_empty() {
        return Ok(());
    }
    let met = manifest
        .meet_min_server_version(CURRENT_VERSION)
        .map_err(|source| EnvError::MinServerVersion {
            id: manifest.id.clone(),
            source,
        })?;
    if !met {
        return Err(EnvError::RequiresVersion {
            min: manifest.min_server_version.clone(),
            id: manifest.id.clone(),
        });
    }
    Ok(())
}

/// Await every future, concurrently, on the current task.
async fn futures_join_all<F: Future<Output = bool>>(futures: impl Iterator<Item = F>) {
    let mut pending: Vec<std::pin::Pin<Box<F>>> = futures.map(Box::pin).collect();
    std::future::poll_fn(|cx| {
        pending.retain_mut(|f| f.as_mut().poll(cx).is_pending());
        if pending.is_empty() {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go's `hash/fnv` New64a over a known input.
    #[test]
    fn fnv64a_matches_go() {
        assert_eq!(fnv64a(b""), 0xcbf2_9ce4_8422_2325u64.to_be_bytes());
        assert_eq!(fnv64a(b"a"), 0xaf63_dc4c_8601_ec8cu64.to_be_bytes());
    }

    #[test]
    fn errno_text_is_gos() {
        let e = io::Error::from_raw_os_error(2);
        assert_eq!(errno(&e), "no such file or directory");
        let e = io::Error::from_raw_os_error(1);
        assert_eq!(errno(&e), "operation not permitted");
    }

    /// `FindManifest`: YAML wins, ids are lowercased, trailing bytes after the JSON value are
    /// ignored as Go's decoder ignores them, and a missing manifest reports no path.
    #[test]
    fn find_manifest_follows_go() {
        let dir = std::env::temp_dir().join(format!("mm-plugin-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let (m, path, err) = find_manifest(&dir);
        assert!(m.is_none() && path.is_empty());
        assert!(
            err.unwrap()
                .to_string()
                .ends_with("plugin.json: no such file or directory")
        );

        std::fs::write(dir.join("plugin.json"), r#"{"id":"MixedCase"} trailing"#).unwrap();
        let (m, path, err) = find_manifest(&dir);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(m.unwrap().id, "mixedcase");
        assert!(path.ends_with("plugin.json"));

        std::fs::write(dir.join("plugin.yaml"), "id: FromYaml\n").unwrap();
        std::fs::write(dir.join("plugin.yml"), "id: FromYml\n").unwrap();
        assert_eq!(
            find_manifest(&dir).0.unwrap().id,
            "fromyml",
            "plugin.yml first"
        );
        std::fs::remove_file(dir.join("plugin.yml")).unwrap();
        assert_eq!(find_manifest(&dir).0.unwrap().id, "fromyaml");

        std::fs::write(dir.join("plugin.yaml"), "id: [unclosed\n").unwrap();
        let (m, path, err) = find_manifest(&dir);
        assert!(m.is_none() && err.is_some() && path.ends_with("plugin.yaml"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
