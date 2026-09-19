//! Business logic ported from `server/channels/app/`.
//!
//! Depends on `mm-store`; knows nothing about HTTP. Handlers live in `mm-api` and call into here,
//! which is what keeps the API layer free of SQL and the store layer free of request semantics.

pub mod access_control_policy;
pub mod agents;
pub mod analytics;
pub mod audit;
pub mod auth;
/// The nil-interface halves of `app/saml.go`, `app/ldap.go` and `app/audit.go` — what the
/// certificate and enterprise-gate routes reach on a build with no SAML or LDAP implementation.
/// Appended rather than filed alphabetically: this list is shared by every worktree.
pub mod auth_certs;
pub mod authorization;
/// Port of `app/board.go` — `CreateBoardChannel`.
pub mod board;
pub mod bot;
/// Port of `app/brand.go` — the brand image read and delete.
pub mod brand;
/// Port of `app/web_broadcast_hooks.go` — the per-connection rewrites the hub runs on the way out.
pub mod broadcast_hooks;
pub mod channel;
pub mod channel_convert;
pub mod channel_create;
pub mod channel_join_request;
pub mod channel_member;
pub mod channel_moderation;
pub mod channel_move;
pub mod channel_view;
pub mod channel_write;
pub mod command;
/// The built-in slash-command registry (`GetCommand` only), `ListAutocompleteCommands` and the
/// dispatch half of `ExecuteCommand`.
pub mod command_provider;
/// Port of `app/command_autocomplete.go` — `GetSuggestions`.
pub mod command_suggestions;
pub mod common_teams;
pub mod config;
pub mod custom_profile_attributes;
pub mod desktop_login;
pub mod draft;
pub mod emoji;
/// Port of the file-backend half of `app/export.go`.
pub mod export;
/// The read side of `app/file.go` — `FileInfo` rows and, through [`filestore`], file bytes.
pub mod file;
pub mod file_search;
/// Port of `UploadFileX` — the single-file upload behind `POST /api/v4/files`.
pub mod file_upload;
/// Port of `platform/shared/filestore` — the local driver, and a refusal for the other two.
pub mod filestore;
/// The one unlicensed read of `app/group.go`, for `members_minus_group_members`.
pub mod group;
/// The supported-locale list, for `users.CreateUser`'s locale reset.
pub mod http_guard;
/// The websocket connection registry and event fan-out — Go's `app/platform` hub.
pub mod hub;
pub mod i18n;
/// The format-detection half of Go's `image.DecodeConfig`.
pub mod imaging;
/// Port of the file-backend half of `app/import.go`.
pub mod import;
pub mod job;
pub mod job_runtime;
pub mod job_scheduler;
pub mod license;
pub mod limits;
pub mod login;
/// The `FirstAdminVisitMarketplace` system row and its broadcast (api4/plugin.go:434-492).
pub mod marketplace_visit;
pub mod mention;
pub mod mfa;
/// Port of Go's `mime.TypeByExtension` and its Unix table loader — `FileInfo.mime_type`.
pub mod mime;
pub mod notification;
pub mod notify_admin;
pub mod oauth;
pub mod onboarding;
pub mod password;
pub mod peer_cache;
pub mod peer_config;
pub mod plugin_install;
pub mod plugins;
pub mod post;
pub mod post_acknowledgement;
pub mod post_create;
pub mod post_rest;
pub mod post_search;
pub mod post_unread;
pub mod post_write;
pub mod preference;
// Appended 2026-09-15: the notice cache and `GetProductNotices`.
pub mod product_notices;
pub mod properties;
pub mod property_hooks;
pub mod reaction;
pub mod report;
pub mod role;
pub mod scheme;
pub mod session;
/// The read side of `app/channel_category.go`.
pub mod sidebar;
pub mod status;
pub mod syncables;
pub mod system;
pub mod team;
pub mod team_member;
pub mod terms_of_service;
pub mod test_notification;
pub mod thread;
pub mod thread_read;
pub mod typing;
/// Port of the two read functions in `app/upload.go`.
pub mod upload;
pub mod usage;
pub mod user;
pub mod user_access_token;
pub mod user_agent;
pub mod user_auth;
pub mod user_convert;
pub mod user_create;
pub mod user_delete;
pub mod user_terms_of_service;
pub mod user_update;
pub mod utils;
/// Port of `app/view.go` — the integrated-boards (kanban view) surface.
pub mod view;
pub mod webhook;
// Appended 2026-09-15: the system-operations family (api4/system.go, elasticsearch.go).
pub mod logs;
pub mod searchengine;
pub mod upgrader;

use mm_store::SqlStore;

use crate::config::Config;

/// The projection [`App::config`] answers with, and the `Configurations.Id` it was loaded from —
/// `None` until the first [`App::refresh_config`], which therefore reloads once unless no row is
/// active at all.
#[derive(Debug)]
struct LoadedConfig {
    id: Option<String>,
    config: std::sync::Arc<Config>,
}

/// Port of `app.App`, as far as the migrated surface needs it.
///
/// Go's `App` is a facade over `Server`/`Platform` holding config, cluster, plugins and the store.
/// The store and the read-only slice of config that migrated code consults are modelled here —
/// the rest arrives when something needs it, rather than as an empty shell that invites guessing
/// about what it holds.
#[derive(Debug, Clone)]
pub struct App {
    store: SqlStore,
    /// Go's `configStore` copy — see [`App::config`] and [`App::refresh_config`]. Shared across
    /// clones, so a reload seen by one request is the configuration every later request reads.
    config: std::sync::Arc<std::sync::RwLock<LoadedConfig>>,
    /// Serialises [`App::refresh_config`] end to end, so a slow reload of an older row cannot
    /// install its document over a newer one a faster call already swapped in.
    config_refresh: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// Shared, because every clone of `App` must publish into the *same* registry of live
    /// connections. `App` is cloned per request by axum's state extractor, and a hub per clone
    /// would mean an event raised by one request reaching none of the sockets.
    hub: std::sync::Arc<crate::hub::Hub>,
    /// The Go server's caches, when something to purge them is installed; see
    /// `crate::peer_cache`.
    peer_cache: Option<std::sync::Arc<dyn crate::peer_cache::PeerCache>>,
    /// Go's `platform.statusCache`, and — like the hub — shared across every clone of `App` so
    /// that a status set by one request is the previous status the next request sees.
    ///
    /// **The cache is the model here, not an optimisation.** Three decisions in the status
    /// setters read the *previous* status and would take a different branch against the table:
    /// the manual-override early return, whether to broadcast at all, and whether the row is
    /// written. See `crate::status` and [D-191].
    status_cache: std::sync::Arc<
        std::sync::RwLock<std::collections::HashMap<String, mm_model::status::Status>>,
    >,
    /// Go's `Server.seenPendingPostIdsCache` (app/post.go:28), shared across every clone of
    /// `App` for the same reason the status cache is: a pending post id claimed by one request
    /// must be the one the retry sees.
    ///
    /// A map rather than an LRU because the eviction policy is not what
    /// `deduplicateCreatePost` reads — the three states (absent, claimed, saved) are, and each
    /// answers differently. Entries carry their own expiry and are dropped on the way past.
    pending_post_ids: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, crate::post_create::PendingPostEntry>>,
    >,
    /// Go's `uploadLockMap` (app/channels.go) — the upload-session ids with a chunk in flight,
    /// shared across every clone so a second chunk for one session is refused whichever request
    /// holds it. See `crate::upload`.
    upload_locks: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Go's `platform.filestore` — the backend every file, image, emoji and brand read goes
    /// through. Built once from the configuration, like Go's, so a route never re-reads
    /// `FileSettings` to decide where to look.
    filestore: crate::filestore::FileBackend,
    /// Go's `platform.exportFilestore` (platform/service.go:397).
    ///
    /// **The same backend as [`App::filestore`] unless `DedicatedExportStore` is set**, which is
    /// the stock configuration. Modelled as its own field rather than as a method that branches,
    /// because that is what Go holds: two names for one object, and the branch taken once at
    /// startup.
    export_filestore: crate::filestore::FileBackend,
    /// The keys a stored or environment licence must verify against — Mattermost's own, or the
    /// operator's override. See [`crate::license`].
    license_keys: crate::license::LicenseKeys,
    /// `MM_LICENSE`, resolved once at construction the way Go resolves it once at startup.
    env_license: crate::license::EnvLicense,
    /// The verified licence for the current `Licenses.Id`, shared across clones so the RSA work
    /// happens once per licence rather than once per request.
    license_cache: std::sync::Arc<crate::license::LicenseCache>,
    /// Go's `Channels.cachedNotices` and the three counts beside it — see
    /// `crate::product_notices`. Shared across clones for the same reason as the hub.
    notices_cache: crate::product_notices::SharedNoticesCache,
    /// The plugin host, shared across clones like the hub: one environment per process. See
    /// `crate::plugins`.
    plugins: std::sync::Arc<crate::plugins::PluginHost>,
    /// The Go server, for the configuration writes this server makes; see `crate::peer_config`.
    peer_config: Option<std::sync::Arc<dyn crate::peer_config::PeerConfig>>,
}

impl App {
    /// An `App` on Go's default configuration.
    ///
    /// Both settings [`Config`] models default to `false` in Go, so this is the correct
    /// construction for any deployment that has not changed them. `main.rs` uses
    /// [`App::with_config`] with [`Config::from_env`] so a configured deployment agrees with the
    /// Go server beside it.
    pub fn new(store: SqlStore) -> Self {
        Self::with_config(store, Config::default())
    }

    pub fn with_config(store: SqlStore, config: Config) -> Self {
        let filestore = crate::filestore::FileBackend::new(
            &crate::filestore::FileBackendSettings::from_file_settings(
                &config.file_driver_name,
                &config.file_directory,
            ),
        );
        // Port of platform/service.go:396-408. `exportFilestore` starts as `filestore` and is
        // replaced only when the dedicated store is configured.
        let export_filestore = if config.dedicated_export_store {
            crate::filestore::FileBackend::new(
                &crate::filestore::FileBackendSettings::from_file_settings(
                    &config.file_export_driver_name,
                    &config.file_export_directory,
                ),
            )
        } else {
            filestore.clone()
        };

        let license_keys = crate::license::LicenseKeys::from_config(&config);
        let env_license = crate::license::EnvLicense::resolve(&config.license, &license_keys);

        Self {
            store,
            config: std::sync::Arc::new(std::sync::RwLock::new(LoadedConfig {
                id: None,
                config: std::sync::Arc::new(config),
            })),
            config_refresh: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            filestore,
            export_filestore,
            license_keys,
            env_license,
            license_cache: std::sync::Arc::new(std::sync::RwLock::new(None)),
            notices_cache: std::sync::Arc::new(std::sync::RwLock::new(
                crate::product_notices::NoticesCache::default(),
            )),
            hub: std::sync::Arc::new(crate::hub::Hub::new()),
            peer_cache: None,
            status_cache: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            pending_post_ids: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            upload_locks: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
            plugins: std::sync::Arc::new(crate::plugins::PluginHost::default()),
            peer_config: None,
        }
    }

    /// Install the configuration writer — the Go server, in `main.rs`. Call before the `App` is
    /// cloned, as [`App::with_peer_cache`].
    pub fn with_peer_config(
        mut self,
        peer: std::sync::Arc<dyn crate::peer_config::PeerConfig>,
    ) -> Self {
        self.peer_config = Some(peer);
        self
    }

    /// The installed configuration writer, if any.
    pub fn peer_config(&self) -> Option<&dyn crate::peer_config::PeerConfig> {
        self.peer_config.as_deref()
    }

    /// Install the plugin host — [`crate::plugins::plugin_host_from_env`] in `main.rs`. Call before
    /// the `App` is cloned, as [`App::with_peer_cache`].
    pub fn with_plugin_host(mut self, host: crate::plugins::PluginHost) -> Self {
        self.plugins = std::sync::Arc::new(host);
        self
    }

    /// Install the purge for the Go server's session cache. Call before the `App` is cloned: a
    /// clone made earlier keeps whatever it had.
    pub fn with_peer_cache(
        mut self,
        peer_cache: std::sync::Arc<dyn crate::peer_cache::PeerCache>,
    ) -> Self {
        self.peer_cache = Some(peer_cache);
        self
    }

    /// The installed purge for the Go server's caches, if any. See `crate::peer_cache`.
    pub fn peer_cache(&self) -> Option<&dyn crate::peer_cache::PeerCache> {
        self.peer_cache.as_deref()
    }

    /// The notice cache — `a.ch.cachedNotices` and its counts.
    pub fn notices_cache(&self) -> &crate::product_notices::SharedNoticesCache {
        &self.notices_cache
    }

    /// Port of `app.App.Srv().Store()`.
    pub fn store(&self) -> &SqlStore {
        &self.store
    }

    /// Port of `app.App.Config()`, narrowed to the settings something ported actually reads.
    ///
    /// A snapshot, as Go's is: `Store.Get` hands out the pointer it holds and `Load` swaps in a
    /// new one rather than mutating it, so a request that reads the configuration twice sees one
    /// document or the other, never a mixture. Holding the `Arc` across an `.await` keeps that
    /// request on the document it started with, which is also Go's behaviour.
    pub fn config(&self) -> std::sync::Arc<Config> {
        let loaded = self
            .config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::sync::Arc::clone(&loaded.config)
    }

    /// Reload the projection when the active `Configurations` row is no longer the one it was
    /// loaded from. Answers whether it reloaded.
    ///
    /// # Why this exists, and what in Go it stands for
    ///
    /// Go's `DatabaseStore` has **no watcher** (config/database.go): a Go server swaps its copy
    /// on its own `SaveConfig`, on `POST /config/reload` (`ReloadConfig` → `configStore.Load`),
    /// and — in a cluster — on the `ConfigChanged` message a peer's `SaveConfig` sends
    /// (platform/config.go:120). This process is that peer, but the cluster bus is private
    /// enterprise code, so the message cannot reach it. The row is what the message would have
    /// announced, and a new row is exactly a changed document: `persist` inserts every change
    /// under a fresh id and writes nothing when the SHA is unchanged. So comparing ids is the
    /// cluster message's analogue, and it is called on the two paths that stand in for its
    /// delivery — after every write request this server answers or forwards, and on a timer
    /// for the writes Go makes that never pass through here.
    ///
    /// # The id is read before the document, and that order is the correctness argument
    ///
    /// A write that lands between the two reads leaves the *older* id beside the *newer*
    /// document, and the next call reloads once more for nothing. The other order would store
    /// the newer id beside the older document, and no later call would ever notice.
    ///
    /// # One refresh at a time
    ///
    /// The after-write hook and the timer can overlap. Unserialised, a call that read the older id
    /// and loaded slowly would swap its document in *after* a faster call installed the newer one,
    /// and this process would run on the older document until the next refresh noticed. The whole
    /// check-load-swap runs under `config_refresh`; a waiter then usually finds the id current and
    /// returns without loading.
    ///
    /// # A document that does not load leaves the old one in force
    ///
    /// As Go's `Store.Load` does: it returns the error before swapping (store.go:260-298). The
    /// error is returned so the caller can log it; the id is not recorded, so the next call
    /// tries again.
    ///
    /// # What is not rebuilt
    ///
    /// The file backends, the licence keys and `MM_LICENSE` are built once in
    /// [`App::with_config`] and stay built. Go's `filestore` and `exportFilestore` are likewise
    /// initialised once (`if ps.filestore == nil`, platform/service.go:385) with no config
    /// listener, and the other two are environment-only.
    #[tracing::instrument(skip_all, fields(reloaded))]
    pub async fn refresh_config(&self) -> Result<bool, crate::config::ConfigError> {
        use mm_store::ConfigStore as _;

        let _refresh = self.config_refresh.lock().await;
        let id = self.store.config().active_id().await?;
        {
            let loaded = self
                .config
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if loaded.id == id {
                tracing::Span::current().record("reloaded", false);
                return Ok(false);
            }
        }
        let config = std::sync::Arc::new(Config::load(self.store.config()).await?);
        let previous = {
            let mut loaded = self
                .config
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::replace(
                &mut *loaded,
                LoadedConfig {
                    id,
                    config: std::sync::Arc::clone(&config),
                },
            )
            .config
        };
        tracing::Span::current().record("reloaded", true);
        // Go's config listeners, of which the plugin host's are the ones ported.
        self.plugins_config_changed(&previous, &config).await;
        Ok(true)
    }

    /// Port of `app.App.FileBackend()` (app/file.go:53).
    pub fn file_backend(&self) -> &crate::filestore::FileBackend {
        &self.filestore
    }

    /// Port of `app.App.ExportFileBackend()` (app/file.go:57).
    pub fn export_file_backend(&self) -> &crate::filestore::FileBackend {
        &self.export_filestore
    }
}
