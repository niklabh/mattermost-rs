//! Business logic ported from `server/channels/app/`.
//!
//! Depends on `mm-store`; knows nothing about HTTP. Handlers live in `mm-api` and call into here,
//! which is what keeps the API layer free of SQL and the store layer free of request semantics.

pub mod authorization;
pub mod channel;
pub mod config;
pub mod password;
pub mod post;
pub mod preference;
pub mod role;
pub mod session;
/// The read side of `app/channel_category.go`.
pub mod sidebar;
pub mod status;
pub mod team;
pub mod user;
pub mod user_terms_of_service;

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
        Self { store, config }
    }

    /// Port of `app.App.Srv().Store()`.
    pub fn store(&self) -> &SqlStore {
        &self.store
    }

    /// Port of `app.App.Config()`, narrowed to the settings something ported actually reads.
    pub fn config(&self) -> &Config {
        &self.config
    }
}
