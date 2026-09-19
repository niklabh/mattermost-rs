//! Prepackaged and transitionally prepackaged plugins (app/plugin.go:903-1242): the bundles a
//! server ships in a `prepackaged_plugins` directory beside it, read once as plugins start.
//!
//! # What start-up does with them
//!
//! [`App::process_prepackaged_plugins`] finds the directory as Go's `fileutils.FindDir` does (the
//! working directory and its ancestors, then the executable's), walks it **recursively**, and
//! pairs every `<id>.tar.gz` with an `<id>.tar.gz.sig` by file name (`getPluginsFromFilePaths`,
//! shared with the file-store sync). Each bundle then:
//!
//! - must have a signature, and it must verify (`verifyPlugin`) — a prepackaged plugin is always
//!   checked, whatever `RequirePluginSignature` says;
//! - is extracted, and its icon read as a `data:` URI when the manifest names an SVG one;
//! - is installed only if `AutomaticPrepackagedPlugins` is on **and** `PluginStates` enables it,
//!   and then only when new or newer than what the plugin directory holds (a same-or-older one is
//!   Go's `skip_installation`, which is not a failure).
//!
//! A bundle that fails any step is logged and left out of both lists. The rest become the
//! environment's prepackaged list, which `GET /plugins/marketplace` merges and a Marketplace
//! install prefers — except the eleven [`TRANSITIONALLY_PREPACKAGED_PLUGINS`], which are never on
//! it: an enabled one that improves on what the file-store sync installed goes on the transitional
//! list instead, and [`App::persist_transitionally_prepackaged_plugins`] writes it (bundle and
//! signature) to the file store, so it survives the release that stops shipping it.
//!
//! # Where this differs from Go, deliberately or by necessity
//!
//! - Go processes the bundles in goroutines and builds the list in completion order, so two
//!   bundles sharing an id come back in either order, and `getPrepackagedPlugin` picks the first.
//!   Here they are processed concurrently but listed by file name (the id), deterministically.
//! - `CommonBaseSearchPaths` ends with the Go server's own source directory
//!   (`server.GetPackagePath`), a path on the build machine; `crate::logs::find_dir` does not
//!   search it.
//! - `persistTransitionallyPrepackagedPlugins` runs only on the cluster leader. Without a cluster
//!   (private code, nil on every public build) `IsLeader` is always true, so it always runs, and
//!   the leader-change listener Go also registers can never fire.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use mm_model::bundle_info::BundleInfo;
use mm_model::go_path;
use mm_model::manifest::StrictVersion;
use mm_plugin::environment::{GoIoError, PrepackagedPlugin};

use crate::App;
use crate::plugin_install::{
    InstallStrategy, PluginSignaturePath, TempDir, plugins_from_file_paths,
};

/// Go's `prepackagedPluginsDir`.
pub const PREPACKAGED_PLUGINS_DIR: &str = "prepackaged_plugins";

/// Go's `transitionallyPrepackagedPlugins` (app/plugin.go:1036): prepackaged now, slated to stop
/// being so.
pub const TRANSITIONALLY_PREPACKAGED_PLUGINS: [&str; 11] = [
    "antivirus",
    "focalboard",
    "mattermost-autolink",
    "com.mattermost.aws-sns",
    "com.mattermost.confluence",
    "com.mattermost.custom-attributes",
    "jenkins",
    "jitsi",
    "com.mattermost.plugin-todo",
    "com.mattermost.welcomebot",
    "com.mattermost.apps",
];

/// Why `processPrepackagedPlugins` gave up, with Go's text.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PrepackagedError {
    #[error("pluginsEnvironment is nil")]
    NoEnvironment,
    #[error("failed to list available plugins: {0}")]
    Available(#[source] mm_plugin::environment::EnvError),
}

/// Why one prepackaged bundle was left out, with Go's text (only ever logged).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PrepackagedPluginError {
    #[error("Failed to open prepackaged plugin {path}: {source}")]
    Open { path: String, source: GoIoError },
    #[error("Failed to create temp dir plugintmp: {0}")]
    TempDir(#[source] std::io::Error),
    #[error("Failed to get prepackaged plugin {path}: {source}")]
    Build { path: String, source: BuildError },
    #[error("Failed to install extracted prepackaged plugin {path}: {source}")]
    Install {
        path: String,
        source: Box<mm_model::utils::AppError>,
    },
}

/// `buildPrepackagedPlugin`'s failures, with Go's text.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BuildError {
    #[error("Prepackaged plugin missing required signature file")]
    MissingSignature,
    #[error("Failed to open prepackaged plugin signature {path}: {source}")]
    OpenSignature { path: String, source: GoIoError },
    #[error(
        "Prepackaged plugin signature verification failed for {bundle} using {signature}: {source}"
    )]
    Verify {
        bundle: String,
        signature: String,
        source: Box<mm_model::utils::AppError>,
    },
    #[error("Failed to extract plugin with path {path}: {source}")]
    Extract {
        path: String,
        source: Box<mm_model::utils::AppError>,
    },
}

/// `filepath.Walk` as `processPrepackagedPlugins` uses it: every path under `root`, `root`
/// first, depth first in lexical order. A symlink is listed and not followed; an entry that cannot
/// be read is listed and not descended into (Go's callback ignores the error it is handed).
fn walk(path: &str, out: &mut Vec<String>) {
    out.push(path.to_owned());
    let is_dir = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
    if !is_dir {
        return;
    }
    let Ok(read) = std::fs::read_dir(path) else {
        return;
    };
    let mut names: Vec<String> = read
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        walk(&go_path::join(&[path, &name]), out);
    }
}

/// `pluginIsTransitionallyPrepackaged`.
pub fn is_transitionally_prepackaged(id: &str) -> bool {
    TRANSITIONALLY_PREPACKAGED_PLUGINS.contains(&id)
}

/// `shouldPersistTransitionallyPrepackagedPlugin` (app/plugin.go:1058): only one `PluginStates`
/// enables, and only when the plugin directory lacks it, or holds an older or unparsable version
/// of it. An unparsable prepackaged version never persists.
pub fn should_persist_transitionally_prepackaged_plugin(
    plugin_states: &std::collections::BTreeMap<String, bool>,
    available: &HashMap<String, BundleInfo>,
    plugin: &PrepackagedPlugin,
) -> bool {
    let Some(manifest) = &plugin.manifest else {
        return false;
    };
    let id = manifest.id.as_str();
    if plugin_states.get(id).copied() != Some(true) {
        tracing::debug!(plugin_id = %id, "Should not persist transitionally prepackaged plugin: not previously enabled");
        return false;
    }
    let Some(existing) = available.get(id).and_then(|b| b.manifest.as_ref()) else {
        tracing::info!(plugin_id = %id, "Should persist transitionally prepackaged plugin: not currently in filestore");
        return true;
    };
    let prepackaged = match StrictVersion::parse_detailed(&manifest.version) {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(plugin_id = %id, error = %err, "Should not persist transitionally prepackged plugin: invalid prepackaged version");
            return false;
        }
    };
    let existing = match StrictVersion::parse_detailed(&existing.version) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(plugin_id = %id, error = %err, "Should persist transitionally prepackged plugin: invalid existing version");
            return true;
        }
    };
    if prepackaged > existing {
        tracing::info!(plugin_id = %id, "Should persist transitionally prepackged plugin: newer version");
        return true;
    }
    tracing::info!(plugin_id = %id, "Should not persist transitionally prepackged plugin: not a newer version");
    false
}

impl App {
    /// Port of `Channels.processPrepackagedPlugins` (app/plugin.go:903); see the module docs.
    #[tracing::instrument(skip(self))]
    pub async fn process_prepackaged_plugins(&self, dir: &str) -> Result<(), PrepackagedError> {
        tracing::info!("Processing prepackaged plugin");
        let (path, found) = crate::logs::find_dir(dir);
        if !found {
            tracing::debug!("No prepackaged plugins directory found");
            return Ok(());
        }
        let path = path.to_string_lossy().into_owned();
        tracing::debug!(prepackaged_plugins_path = %path, "Processing prepackaged plugins in directory");
        let mut paths = Vec::new();
        walk(&path, &mut paths);
        let found = plugins_from_file_paths(&paths);

        // The snapshot before any install: what the file-store sync left.
        let Some(environment) = self.plugins_environment() else {
            return Err(PrepackagedError::NoEnvironment);
        };
        let available: HashMap<String, BundleInfo> = environment
            .available()
            .map_err(PrepackagedError::Available)?
            .into_iter()
            .filter_map(|b| Some((b.manifest.as_ref()?.id.clone(), b)))
            .collect();

        let processed = found.values().map(|p| async move {
            match self.process_prepackaged_plugin(p).await {
                Ok(plugin) => Some(plugin),
                Err(err) => {
                    tracing::error!(bundle_path = %p.bundle_path, error = %err, "Failed to install prepackaged plugin");
                    None
                }
            }
        });
        let processed = join_all_values(processed.collect()).await;

        let config = self.config();
        let mut prepackaged = Vec::new();
        let mut transitional = Vec::new();
        for plugin in processed.into_iter().flatten() {
            let id = plugin.manifest.as_ref().map_or("", |m| m.id.as_str());
            if is_transitionally_prepackaged(id) {
                if should_persist_transitionally_prepackaged_plugin(
                    &config.plugin_states,
                    &available,
                    &plugin,
                ) {
                    transitional.push(plugin);
                }
            } else {
                prepackaged.push(plugin);
            }
        }
        environment.set_prepackaged_plugins(prepackaged, transitional);
        Ok(())
    }

    /// Port of `processPrepackagedPlugin` (app/plugin.go:988).
    async fn process_prepackaged_plugin(
        &self,
        path: &PluginSignaturePath,
    ) -> Result<PrepackagedPlugin, PrepackagedPluginError> {
        tracing::info!(bundle_path = %path.bundle_path, signature_path = %path.signature_path, "Processing prepackaged plugin");
        let bundle =
            std::fs::read(&path.bundle_path).map_err(|e| PrepackagedPluginError::Open {
                path: path.bundle_path.clone(),
                source: GoIoError::new("open", Path::new(&path.bundle_path), e),
            })?;
        let tmp = TempDir::new().map_err(PrepackagedPluginError::TempDir)?;
        let (plugin, plugin_dir) = self
            .build_prepackaged_plugin(path, &bundle, tmp.path())
            .await
            .map_err(|source| PrepackagedPluginError::Build {
                path: path.bundle_path.clone(),
                source,
            })?;
        let Some(manifest) = plugin.manifest.clone() else {
            return Ok(plugin);
        };

        let config = self.config();
        if !config.plugin_automatic_prepackaged_plugins {
            tracing::info!(plugin_id = %manifest.id, "Not installing prepackaged plugin: automatic prepackaged plugins disabled");
            return Ok(plugin);
        }
        if config.plugin_states.get(&manifest.id).copied() != Some(true) {
            tracing::info!(plugin_id = %manifest.id, "Not installing prepackaged plugin: not previously enabled");
            return Ok(plugin);
        }
        if let Err(err) = self
            .install_extracted_plugin(manifest, &plugin_dir, InstallStrategy::OnlyIfNewOrUpgrade)
            .await
            && err.id != "app.plugin.skip_installation.app_error"
        {
            return Err(PrepackagedPluginError::Install {
                path: path.bundle_path.clone(),
                source: err,
            });
        }
        Ok(plugin)
    }

    /// Port of `buildPrepackagedPlugin` (app/plugin.go:1184): the signature is required and must
    /// verify, then the bundle is extracted into `tmp`. Answers the plugin and the directory it
    /// was extracted to. An icon that cannot be read is logged, and the plugin has none.
    async fn build_prepackaged_plugin(
        &self,
        path: &PluginSignaturePath,
        bundle: &[u8],
        tmp: &Path,
    ) -> Result<(PrepackagedPlugin, PathBuf), BuildError> {
        if path.signature_path.is_empty() {
            return Err(BuildError::MissingSignature);
        }
        let signature =
            std::fs::read(&path.signature_path).map_err(|e| BuildError::OpenSignature {
                path: path.signature_path.clone(),
                source: GoIoError::new("open", Path::new(&path.signature_path), e),
            })?;
        self.verify_plugin(bundle, &signature)
            .await
            .map_err(|source| BuildError::Verify {
                bundle: path.bundle_path.clone(),
                signature: path.signature_path.clone(),
                source,
            })?;
        let (manifest, plugin_dir) =
            crate::plugin_install::extract_plugin(bundle, tmp).map_err(|source| {
                BuildError::Extract {
                    path: path.bundle_path.clone(),
                    source,
                }
            })?;
        let mut icon_data = String::new();
        if !manifest.icon_path.is_empty() {
            let icon = go_path::join(&[&plugin_dir.to_string_lossy(), &manifest.icon_path]);
            match crate::marketplace::get_icon(&icon) {
                Ok(data) => icon_data = data,
                Err(err) => {
                    tracing::warn!(icon_path = %manifest.icon_path, error = %err, "Error loading local plugin icon");
                }
            }
        }
        Ok((
            PrepackagedPlugin {
                path: path.bundle_path.clone(),
                signature_path: path.signature_path.clone(),
                manifest: Some(manifest),
                icon_data,
            },
            plugin_dir,
        ))
    }

    /// Port of `persistTransitionallyPrepackagedPlugins` (app/plugin.go:1122): write each plugin
    /// on the transitional list to the file store, bundle and signature, then clear the list, so
    /// a failure is not retried until the next start. Always the leader here; see the module docs.
    #[tracing::instrument(skip(self))]
    pub async fn persist_transitionally_prepackaged_plugins(&self) {
        let Some(environment) = self.plugins_environment() else {
            tracing::debug!(
                "Not persisting transitionally prepackaged plugins: no plugin environment"
            );
            return;
        };
        let plugins = environment.transitionally_prepackaged_plugins();
        if plugins.is_empty() {
            tracing::debug!("Not persisting transitionally prepackaged plugins: none found");
            return;
        }
        let persists = plugins.iter().map(|p| async move {
            let Some(manifest) = &p.manifest else {
                return;
            };
            tracing::info!(plugin_id = %manifest.id, version = %manifest.version, bundle_path = %p.path, signature_path = %p.signature_path, "Persisting transitionally prepackaged plugin");
            let bundle = match std::fs::read(&p.path) {
                Ok(bundle) => bundle,
                Err(err) => {
                    tracing::error!(plugin_id = %manifest.id, error = %err, "Failed to read transitionally prepackaged plugin");
                    return;
                }
            };
            let signature = match std::fs::read(&p.signature_path) {
                Ok(signature) => signature,
                Err(err) => {
                    tracing::error!(plugin_id = %manifest.id, error = %err, "Failed to read transitionally prepackaged plugin signature");
                    return;
                }
            };
            if let Err(err) = self
                .install_plugin_to_filestore(manifest, &bundle, Some(&signature))
                .await
            {
                tracing::error!(plugin_id = %manifest.id, error = %err, "Failed to persist transitionally prepackaged plugin");
            }
        });
        crate::plugins::join_all(persists.collect()).await;
        environment.clear_transitionally_prepackaged_plugins();
        tracing::info!("Finished persisting transitionally prepackaged plugins");
    }
}

/// Await every future concurrently, keeping their outputs in order.
async fn join_all_values<F: std::future::Future>(futures: Vec<F>) -> Vec<F::Output> {
    let mut slots: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
    let mut pending: Vec<(usize, std::pin::Pin<Box<F>>)> =
        futures.into_iter().map(Box::pin).enumerate().collect();
    std::future::poll_fn(|cx| {
        pending.retain_mut(|(i, f)| match f.as_mut().poll(cx) {
            std::task::Poll::Ready(v) => {
                slots[*i] = Some(v);
                false
            }
            std::task::Poll::Pending => true,
        });
        if pending.is_empty() {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
    slots.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `filepath.Walk`'s order: the root, then depth first by name, a symlinked directory listed
    /// and not entered.
    #[test]
    fn walk_lists_as_filepath_walk_does() {
        let root = std::env::temp_dir().join(format!("mm-app-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("b/inner")).unwrap();
        std::fs::write(root.join("a.tar.gz"), b"").unwrap();
        std::fs::write(root.join("b/inner/c.tar.gz"), b"").unwrap();
        std::fs::write(root.join("b.tar.gz"), b"").unwrap();
        std::os::unix::fs::symlink(root.join("b"), root.join("link")).unwrap();
        let r = root.to_string_lossy().into_owned();
        let mut out = Vec::new();
        walk(&r, &mut out);
        let rel: Vec<String> = out
            .iter()
            .map(|p| p.strip_prefix(&r).unwrap_or(p).to_owned())
            .collect();
        assert_eq!(
            rel,
            [
                "",
                "/a.tar.gz",
                "/b",
                "/b/inner",
                "/b/inner/c.tar.gz",
                "/b.tar.gz",
                "/link"
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn plugin(id: &str, version: &str) -> PrepackagedPlugin {
        PrepackagedPlugin {
            manifest: Some(mm_model::manifest::Manifest {
                id: id.into(),
                version: version.into(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn installed(id: &str, version: &str) -> (String, BundleInfo) {
        (
            id.to_owned(),
            BundleInfo {
                manifest: plugin(id, version).manifest,
                ..Default::default()
            },
        )
    }

    /// Each branch of `shouldPersistTransitionallyPrepackagedPlugin`.
    #[test]
    fn should_persist_follows_go() {
        let states: std::collections::BTreeMap<String, bool> =
            [("jitsi".to_owned(), true), ("jenkins".to_owned(), false)].into();
        let none = HashMap::new();
        assert!(
            !should_persist_transitionally_prepackaged_plugin(
                &states,
                &none,
                &plugin("jenkins", "1.0.0")
            ),
            "disabled"
        );
        assert!(
            !should_persist_transitionally_prepackaged_plugin(
                &states,
                &none,
                &plugin("antivirus", "1.0.0")
            ),
            "no state"
        );
        assert!(
            should_persist_transitionally_prepackaged_plugin(
                &states,
                &none,
                &plugin("jitsi", "1.0.0")
            ),
            "not installed"
        );
        let older: HashMap<_, _> = [installed("jitsi", "0.9.0")].into();
        assert!(
            should_persist_transitionally_prepackaged_plugin(
                &states,
                &older,
                &plugin("jitsi", "1.0.0")
            ),
            "newer"
        );
        let same: HashMap<_, _> = [installed("jitsi", "1.0.0")].into();
        assert!(
            !should_persist_transitionally_prepackaged_plugin(
                &states,
                &same,
                &plugin("jitsi", "1.0.0")
            ),
            "same version"
        );
        let newer: HashMap<_, _> = [installed("jitsi", "2.0.0")].into();
        assert!(
            !should_persist_transitionally_prepackaged_plugin(
                &states,
                &newer,
                &plugin("jitsi", "1.0.0")
            ),
            "older"
        );
        assert!(
            !should_persist_transitionally_prepackaged_plugin(
                &states,
                &older,
                &plugin("jitsi", "v1.0.0")
            ),
            "an invalid prepackaged version"
        );
        let invalid: HashMap<_, _> = [installed("jitsi", "1.0")].into();
        assert!(
            should_persist_transitionally_prepackaged_plugin(
                &states,
                &invalid,
                &plugin("jitsi", "0.0.1")
            ),
            "an invalid installed version"
        );
    }

    #[test]
    fn the_transitional_list_is_gos() {
        assert!(is_transitionally_prepackaged("com.mattermost.apps"));
        assert!(is_transitionally_prepackaged("jitsi"));
        assert!(!is_transitionally_prepackaged("playbooks"));
        assert!(
            !is_transitionally_prepackaged("Jitsi"),
            "ids are compared exactly"
        );
    }
}
