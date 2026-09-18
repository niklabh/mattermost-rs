//! The plugin host in the app: when this process hosts plugins, which it does, and what the app
//! asks of the environment (app/plugin.go, app/plugin_statuses.go, and the plugin half of
//! app/channels.go's config listener).
//!
//! # One host at a time
//!
//! Go and this server share a database but not a cluster bus, so two hosts would behave like two
//! HA nodes that cannot hear each other (docs/PLUGIN_PLAN.md, D6). The switch is
//! `MMRS_PLUGIN_HOST`: `go`, the default, leaves the environment unstarted here and every plugin
//! route forwarded; `rust` starts it, and the plugin routes this server has ported answer from it.
//! [`PluginHost::hosted`] is that switch; nothing else here runs when it is off.
//!
//! # What is not ported yet
//!
//! - Signature checks on the bundles `syncPlugins` installs (see `crate::plugin_install`).
//! - Prepackaged and transitionally prepackaged plugins, the health-check job and the cluster
//!   leader listener.
//! - The plugin API itself: [`AppPluginApi`] answers every method with the typed
//!   not-implemented error (plugin plan Phase 6), and [`AppPluginDriver`] likewise.
//!
//! Each is a `docs/TECH_DEBT.md` entry. The cluster branches are Go's nil cluster: there is no
//! cluster here, so a status carries `cluster_id: ""` and no peer statuses are merged.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mm_model::manifest::Manifest;
use mm_model::plugin_status::{PLUGIN_STATE_RUNNING, PluginStatus, PluginStatuses};
use mm_model::utils::AppError;
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_PLUGIN_DISABLED, WEBSOCKET_EVENT_PLUGIN_ENABLED,
    WEBSOCKET_EVENT_PLUGIN_STATUSES_CHANGED, WebSocketEvent,
};
use mm_plugin::environment::Environment;
use mm_plugin::rpc::hook_id;

use crate::App;
use crate::config::Config;

/// Go's `model.PluginIdApps`, whose state follows `FeatureFlags.AppsEnabled`.
pub(crate) const PLUGIN_ID_APPS: &str = "com.mattermost.apps";

/// The server API a plugin is served. Every method answers Go's not-implemented error until the
/// plugin plan's Phase 6 ports them.
pub struct AppPluginApi;
impl mm_plugin::rpc::PluginApi for AppPluginApi {}
impl mm_plugin::rpc::PluginApiStreams for AppPluginApi {}
impl mm_plugin::rpc::PluginApiHttp for AppPluginApi {}

/// The database a plugin queries through. Not ported yet; see [`AppPluginApi`].
pub struct AppPluginDriver;
impl mm_plugin::rpc::Driver for AppPluginDriver {}

/// The environment this server runs its plugins in.
pub type PluginsEnvironment = Environment<AppPluginApi, AppPluginDriver>;

/// Go's `Channels.pluginsEnvironment` and `pluginsLock`, plus the switch that decides whether
/// this process hosts plugins at all.
#[derive(Default)]
pub struct PluginHost {
    hosted: bool,
    environment: std::sync::RwLock<Option<Arc<PluginsEnvironment>>>,
    /// Serialises `initPlugins`, `syncPluginsActiveState` and `ShutDownPlugins`: Go runs each
    /// under its config listener, one change at a time.
    lifecycle: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for PluginHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginHost")
            .field("hosted", &self.hosted)
            .field("started", &self.get().is_some())
            .finish_non_exhaustive()
    }
}

impl PluginHost {
    /// A host that runs plugins here (`MMRS_PLUGIN_HOST=rust`).
    pub fn hosting() -> Self {
        Self {
            hosted: true,
            ..Self::default()
        }
    }

    /// Whether this process hosts plugins.
    pub fn hosted(&self) -> bool {
        self.hosted
    }

    fn get(&self) -> Option<Arc<PluginsEnvironment>> {
        self.environment
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set(&self, environment: Option<Arc<PluginsEnvironment>>) {
        *self
            .environment
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = environment;
    }
}

/// `MMRS_PLUGIN_HOST`: `rust` hosts plugins here; anything else, or nothing, leaves them to Go.
pub fn plugin_host_from_env() -> PluginHost {
    match std::env::var("MMRS_PLUGIN_HOST").as_deref() {
        Ok("rust") => PluginHost::hosting(),
        _ => PluginHost::default(),
    }
}

/// Whether any `PluginSettings` value this server models changed: Go re-runs `initPlugins`, or
/// shuts plugins down, on any `PluginSettings.*` diff (app/channels.go:280-303).
fn plugin_settings_changed(old: &Config, new: &Config) -> bool {
    old.plugin_enable != new.plugin_enable
        || old.plugin_directory != new.plugin_directory
        || old.plugin_client_directory != new.plugin_client_directory
        || old.plugin_states != new.plugin_states
        || old.plugin_enable_marketplace != new.plugin_enable_marketplace
}

/// `os.Mkdir(dir, 0744)`, tolerating an existing directory.
fn make_plugin_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    match std::fs::DirBuilder::new().mode(0o744).create(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

impl App {
    /// The plugin host: [`PluginHost::hosted`] says whether this process runs plugins.
    pub fn plugin_host(&self) -> &PluginHost {
        &self.plugins
    }

    /// Port of `Channels.GetPluginsEnvironment` (app/plugin.go:47): the environment when plugins
    /// are enabled and started, and `None` otherwise.
    pub fn plugins_environment(&self) -> Option<Arc<PluginsEnvironment>> {
        if !self.config().plugin_enable {
            return None;
        }
        self.plugins.get()
    }

    /// Port of `Channels.initPlugins` (app/plugin.go:172), without the file-store sync and the
    /// prepackaged plugins (see the module docs): make the two directories, start the
    /// environment, and activate what `PluginStates` enables. A second call only re-syncs.
    #[tracing::instrument(skip(self))]
    pub async fn init_plugins(&self, plugin_dir: &Path, webapp_plugin_dir: &Path) {
        if !self.plugins.hosted {
            return;
        }
        let lifecycle = self.plugins.lifecycle.lock().await;
        if self.plugins.get().is_some() || !self.config().plugin_enable {
            drop(lifecycle);
            self.sync_plugins_active_state().await;
            return;
        }
        tracing::info!("Starting up plugins");
        for dir in [plugin_dir, webapp_plugin_dir] {
            if let Err(err) = make_plugin_dir(dir) {
                tracing::error!(error = %err, "Failed to start up plugins");
                return;
            }
        }
        let environment = Environment::new(
            Box::new(|_: &Manifest| Arc::new(AppPluginApi)),
            Arc::new(AppPluginDriver),
            PathBuf::from(plugin_dir),
            PathBuf::from(webapp_plugin_dir),
        );
        self.plugins.set(Some(Arc::new(environment)));
        drop(lifecycle);
        if let Err(err) = self.sync_plugins().await {
            tracing::error!(error = %err, "Failed to sync plugins from the file store");
        }
        self.sync_plugins_active_state().await;
    }

    /// Port of `Channels.syncPluginsActiveState` (app/plugin.go:75): deactivate every available
    /// plugin `PluginStates` disables and activate every one it enables, concurrently, then tell
    /// system admins the statuses changed. With plugins off, shut the environment down instead.
    #[tracing::instrument(skip(self))]
    pub async fn sync_plugins_active_state(&self) {
        let _lifecycle = self.plugins.lifecycle.lock().await;
        let Some(environment) = self.plugins.get() else {
            return;
        };
        let config = self.config();
        if !config.plugin_enable {
            environment.shutdown().await;
            self.notify_plugin_statuses_changed().await;
            return;
        }
        let available = match environment.available() {
            Ok(available) => available,
            Err(err) => {
                tracing::error!(error = %err, "Unable to get available plugins");
                return;
            }
        };

        let mut disabled = Vec::new();
        let mut enabled = Vec::new();
        for plugin in available {
            let Some(manifest) = plugin.manifest.clone() else {
                continue;
            };
            let mut on = config
                .plugin_states
                .get(&manifest.id)
                .copied()
                .unwrap_or(false);
            // getPluginStateOverride: the Apps plugin follows its feature flag.
            if manifest.id == PLUGIN_ID_APPS && !config.feature_flag_apps_enabled {
                on = false;
            }
            if on {
                enabled.push((manifest, plugin.path));
            } else {
                disabled.push(manifest);
            }
        }

        let deactivations = disabled.into_iter().map(|manifest| {
            let environment = Arc::clone(&environment);
            async move {
                let deactivated = environment.deactivate(&manifest.id).await;
                if deactivated && manifest.has_client() {
                    self.publish_plugin_manifest(WEBSOCKET_EVENT_PLUGIN_DISABLED, &manifest)
                        .await;
                }
            }
        });
        let activations = enabled.into_iter().map(|(manifest, path)| {
            let environment = Arc::clone(&environment);
            async move {
                match environment.activate(&manifest.id).await {
                    Ok(Some(updated)) => self.notify_plugin_enabled(&environment, &updated).await,
                    Ok(None) => {}
                    Err(err) => tracing::error!(
                        plugin_id = %manifest.id,
                        bundle_path = %path,
                        error = %err,
                        "Unable to activate plugin"
                    ),
                }
            }
        });
        tokio::join!(
            join_all(deactivations.collect()),
            join_all(activations.collect())
        );
        self.notify_plugin_statuses_changed().await;
    }

    /// Port of `Channels.ShutDownPlugins` (app/plugin.go:347).
    #[tracing::instrument(skip(self))]
    pub async fn shut_down_plugins(&self) {
        let _lifecycle = self.plugins.lifecycle.lock().await;
        let Some(environment) = self.plugins.get() else {
            return;
        };
        tracing::info!("Shutting down plugins");
        environment.shutdown().await;
        self.plugins.set(None);
    }

    /// The plugin half of the config listeners (app/channels.go:280-303 and app/plugin.go:236):
    /// a `PluginSettings` change starts or stops the host; any change re-syncs a running host
    /// when `Enable` held still, and tells every plugin through `OnConfigurationChange`.
    pub async fn plugins_config_changed(&self, old: &Config, new: &Config) {
        if !self.plugins.hosted {
            return;
        }
        if plugin_settings_changed(old, new) {
            if new.plugin_enable {
                self.init_plugins(
                    Path::new(&new.plugin_directory),
                    Path::new(&new.plugin_client_directory),
                )
                .await;
            } else {
                self.shut_down_plugins().await;
            }
        }
        let Some(environment) = self.plugins.get() else {
            return;
        };
        if old.plugin_enable == new.plugin_enable {
            self.sync_plugins_active_state().await;
        }
        environment
            .run_multi_plugin_hook(hook_id::ON_CONFIGURATION_CHANGE, |hooks, _| async move {
                let returns = hooks
                    .on_configuration_change(
                        mm_plugin::wire::plugin::Z_OnConfigurationChangeArgs {},
                    )
                    .await;
                if let Some(err) = mm_plugin::error::decodable_error(returns.a.as_ref()) {
                    tracing::error!(error = %err.go_error(), "Plugin OnConfigurationChange hook failed");
                }
                true
            })
            .await;
    }

    /// Port of `Channels.GetPluginStatuses` (app/plugin_statuses.go:44).
    pub fn get_plugin_statuses(&self) -> Result<PluginStatuses, Box<AppError>> {
        let Some(environment) = self.plugins_environment() else {
            return Err(AppError::boxed(
                "GetPluginStatuses",
                "app.plugin.disabled.app_error",
                None,
                "",
                501,
            ));
        };
        let mut statuses = environment.statuses().map_err(|err| {
            Box::new(
                AppError::new(
                    "GetPluginStatuses",
                    "app.plugin.get_statuses.app_error",
                    None,
                    "",
                    500,
                )
                .wrap(err),
            )
        })?;
        // No cluster: Go's `ClusterId = ""` branch.
        for status in &mut statuses.0 {
            status.cluster_id.clear();
        }
        Ok(statuses)
    }

    /// Port of `getClusterPluginStatuses` (app/plugin_statuses.go:77). The cluster branch needs
    /// a cluster, which is private code and nil on every build of the public tree.
    pub fn get_cluster_plugin_statuses(&self) -> Result<PluginStatuses, Box<AppError>> {
        self.get_plugin_statuses()
    }

    /// Port of `Channels.GetPluginStatus` (app/plugin_statuses.go:13).
    pub fn get_plugin_status(&self, id: &str) -> Result<PluginStatus, Box<AppError>> {
        let where_ = "GetPluginStatus";
        let Some(environment) = self.plugins_environment() else {
            return Err(AppError::boxed(
                where_,
                "app.plugin.disabled.app_error",
                None,
                "",
                501,
            ));
        };
        let statuses = environment.statuses().map_err(|err| {
            Box::new(
                AppError::new(where_, "app.plugin.get_statuses.app_error", None, "", 500).wrap(err),
            )
        })?;
        statuses
            .0
            .into_iter()
            .find(|s| s.plugin_id == id)
            .ok_or_else(|| {
                AppError::boxed(where_, "app.plugin.not_installed.app_error", None, "", 404)
            })
    }

    /// Port of `notifyPluginStatusesChanged` (app/plugin_statuses.go:99): a signal to system
    /// admins, carrying an always-empty `plugin_statuses` for clients that index it.
    pub(crate) async fn notify_plugin_statuses_changed(&self) {
        let mut message = WebSocketEvent::new(
            WEBSOCKET_EVENT_PLUGIN_STATUSES_CHANGED,
            "",
            "",
            "",
            None,
            "",
        );
        message.add("plugin_statuses", serde_json::Value::Array(Vec::new()));
        if let Some(broadcast) = message.broadcast.as_mut() {
            broadcast.contains_sensitive_data = true;
            broadcast.reliable_cluster_send = true;
        }
        self.publish(message).await;
    }

    /// Port of `notifyPluginEnabled` (app/plugin.go:818), with no cluster to ask: announce a
    /// running plugin with a client component unless its status disagrees on the version.
    pub(crate) async fn notify_plugin_enabled(
        &self,
        environment: &PluginsEnvironment,
        manifest: &Manifest,
    ) {
        if !manifest.has_client() || !environment.is_active(&manifest.id) {
            return;
        }
        let status = match self.get_plugin_status(&manifest.id) {
            Ok(status) => status,
            Err(err) => {
                tracing::error!(error = %err, plugin_id = %manifest.id, "Failed to notify cluster on plugin enable");
                return;
            }
        };
        if status.version != manifest.version {
            tracing::debug!(plugin_id = %manifest.id, "Not ready to notify webclients");
            return;
        }
        self.publish_plugin_manifest(WEBSOCKET_EVENT_PLUGIN_ENABLED, manifest)
            .await;
    }

    async fn publish_plugin_manifest(&self, event: &str, manifest: &Manifest) {
        let mut message = WebSocketEvent::new(event, "", "", "", None, "");
        match serde_json::to_value(manifest.client_manifest()) {
            Ok(value) => message.add("manifest", value),
            Err(err) => {
                tracing::warn!(error = %err, "Failed to encode a plugin manifest");
                return;
            }
        }
        self.publish(message).await;
    }

    /// Port of `App.GetPlugins` (app/plugin.go:506): every available plugin's manifest, split
    /// by whether it is running, in plugin-directory order.
    pub fn get_plugins(&self) -> Result<mm_model::manifest::PluginsResponse, Box<AppError>> {
        let Some(environment) = self.plugins_environment() else {
            return Err(AppError::boxed(
                "GetPlugins",
                "app.plugin.disabled.app_error",
                None,
                "",
                501,
            ));
        };
        let available = environment.available().map_err(|err| {
            Box::new(
                AppError::new(
                    "GetPlugins",
                    "app.plugin.get_plugins.app_error",
                    None,
                    "",
                    500,
                )
                .wrap(err),
            )
        })?;
        let (mut active, mut inactive) = (Vec::new(), Vec::new());
        for plugin in available {
            let Some(manifest) = plugin.manifest else {
                continue;
            };
            let running = environment.is_active(&manifest.id);
            let info = mm_model::manifest::PluginInfo { manifest };
            if running {
                active.push(info);
            } else {
                inactive.push(info);
            }
        }
        Ok(mm_model::manifest::PluginsResponse {
            active: Some(active),
            inactive: Some(inactive),
        })
    }

    /// Port of `App.GetActivePluginManifests` (app/plugin.go:394): the running plugins'
    /// manifests as they were registered, so a webapp's carries its bundle hash. Go ranges a
    /// `sync.Map`, whose order is unspecified; here it is by id.
    pub fn get_active_plugin_manifests(&self) -> Result<Vec<Manifest>, Box<AppError>> {
        let Some(environment) = self.plugins_environment() else {
            return Err(AppError::boxed(
                "GetActivePluginManifests",
                "app.plugin.disabled.app_error",
                None,
                "",
                501,
            ));
        };
        Ok(environment
            .active()
            .into_iter()
            .filter_map(|b| b.manifest)
            .collect())
    }

    /// Whether a plugin is running, by its status (app/plugin.go:1259).
    pub fn is_plugin_active(&self, id: &str) -> Result<bool, Box<AppError>> {
        Ok(self.get_plugin_status(id)?.state == PLUGIN_STATE_RUNNING)
    }
}

/// Await every future concurrently on the current task: Go's `WaitGroup` over goroutines.
pub(crate) async fn join_all<F: std::future::Future<Output = ()>>(futures: Vec<F>) {
    let mut pending: Vec<std::pin::Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
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

    #[test]
    fn any_modelled_plugin_setting_is_a_change() {
        let old = Config::default();
        assert!(!plugin_settings_changed(&old, &old.clone()));
        let mut new = old.clone();
        new.plugin_states.insert("x".into(), true);
        assert!(plugin_settings_changed(&old, &new));
        let mut new = old.clone();
        new.plugin_client_directory = "/elsewhere".into();
        assert!(plugin_settings_changed(&old, &new));
        let mut new = old.clone();
        new.show_full_name = !new.show_full_name;
        assert!(!plugin_settings_changed(&old, &new), "not a plugin setting");
    }

    #[test]
    fn the_host_is_go_unless_the_switch_says_rust() {
        assert!(!PluginHost::default().hosted());
        assert!(PluginHost::hosting().hosted());
    }
}
