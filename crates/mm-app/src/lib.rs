//! Business logic ported from `server/channels/app/`.
//!
//! Depends on `mm-store`; knows nothing about HTTP. Handlers live in `mm-api` and call into here,
//! which is what keeps the API layer free of SQL and the store layer free of request semantics.

pub mod password;
pub mod preference;
pub mod session;
pub mod team;
pub mod user;

use mm_store::SqlStore;

/// Port of `app.App`, as far as the migrated surface needs it.
///
/// Go's `App` is a facade over `Server`/`Platform` holding config, cluster, plugins and the store.
/// Only the store is modelled here — the rest arrives when something needs it, rather than as an
/// empty shell that invites guessing about what it holds.
#[derive(Debug, Clone)]
pub struct App {
    store: SqlStore,
}

impl App {
    pub fn new(store: SqlStore) -> Self {
        Self { store }
    }

    /// Port of `app.App.Srv().Store()`.
    pub fn store(&self) -> &SqlStore {
        &self.store
    }
}
