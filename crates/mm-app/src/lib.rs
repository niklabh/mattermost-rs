//! Business logic ported from `server/channels/app/`.
//!
//! Depends on `mm-store`; knows nothing about HTTP. Handlers live in `mm-api` and call into here,
//! which is what keeps the API layer free of SQL and the store layer free of request semantics.

pub mod audit;
pub mod authorization;
pub mod bot;
/// Port of `app/brand.go` — the brand image read and delete.
pub mod brand;
pub mod channel;
pub mod channel_member;
pub mod channel_view;
pub mod channel_write;
pub mod command;
pub mod common_teams;
pub mod config;
pub mod draft;
pub mod emoji;
/// Port of the file-backend half of `app/export.go`.
pub mod export;
/// The read side of `app/file.go` — `FileInfo` rows and, through [`filestore`], file bytes.
pub mod file;
/// Port of `platform/shared/filestore` — the local driver, and a refusal for the other two.
pub mod filestore;
/// The websocket connection registry and event fan-out — Go's `app/platform` hub.
pub mod hub;
/// The format-detection half of Go's `image.DecodeConfig`.
pub mod imaging;
/// Port of the file-backend half of `app/import.go`.
pub mod import;
pub mod job;
pub mod license;
pub mod limits;
pub mod oauth;
pub mod password;
pub mod post;
pub mod preference;
pub mod reaction;
pub mod report;
pub mod role;
pub mod scheme;
pub mod session;
/// The read side of `app/channel_category.go`.
pub mod sidebar;
pub mod status;
pub mod system;
pub mod team;
pub mod terms_of_service;
pub mod thread;
/// Port of the two read functions in `app/upload.go`.
pub mod upload;
pub mod usage;
pub mod user;
pub mod user_access_token;
pub mod user_terms_of_service;
pub mod utils;
pub mod webhook;

use mm_store::SqlStore;

use crate::config::Config;

/// Port of `app.App`, as far as the migrated surface needs it.
///
/// Go's `App` is a facade over `Server`/`Platform` holding config, cluster, plugins and the store.
/// The store and the read-only slice of config that migrated code consults are modelled here —
/// the rest arrives when something needs it, rather than as an empty shell that invites guessing
/// about what it holds.
#[derive(Debug, Clone)]
pub struct App {
    store: SqlStore,
    config: Config,
    /// Shared, because every clone of `App` must publish into the *same* registry of live
    /// connections. `App` is cloned per request by axum's state extractor, and a hub per clone
    /// would mean an event raised by one request reaching none of the sockets.
    hub: std::sync::Arc<crate::hub::Hub>,
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

        Self {
            store,
            config,
            filestore,
            export_filestore,
            hub: std::sync::Arc::new(crate::hub::Hub::new()),
            status_cache: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
        }
    }

    /// Port of `app.App.Srv().Store()`.
    pub fn store(&self) -> &SqlStore {
        &self.store
    }

    /// Port of `app.App.Config()`, narrowed to the settings something ported actually reads.
    pub fn config(&self) -> &Config {
        &self.config
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
