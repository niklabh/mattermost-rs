//! Persistence layer ported from `server/channels/store/`.
//!
//! Depends on `mm-model` only. Store traits use native async-in-trait (RPITIT).
//!
//! # The database is shared, and the Go server owns it
//!
//! Both servers read and write one Postgres database during the migration. The Go server owns the
//! schema: it runs the migrations, and this crate only ever reads a shape someone else created.
//! Nothing here may issue DDL, and no migration tool may be pointed at this database from the
//! Rust side — the two servers would race, and Go's migrations are the reference.
//!
//! Column names are therefore not ours to choose. Go declares them in CamelCase and Postgres
//! folds unquoted identifiers to lower case, so `CreateAt` is `createat` on the wire to the
//! driver. Queries here spell them the way the database does.

pub mod channel_store;
pub mod emoji_store;
pub mod error;
pub mod file_info_store;
pub mod post_store;
pub mod preference_store;
pub mod reaction_store;
pub mod role_store;
pub mod scheme_store;
pub mod session_store;
pub mod status_store;
pub mod team_store;
pub mod user_store;
pub mod user_terms_of_service_store;

pub use channel_store::{ChannelStore, SqlChannelStore};
pub use emoji_store::{EmojiStore, SqlEmojiStore};
pub use error::StoreError;
pub use file_info_store::{FileInfoStore, SqlFileInfoStore};
pub use post_store::{PostStore, SqlPostStore};
pub use preference_store::{PreferenceStore, SqlPreferenceStore};
pub use reaction_store::{ReactionStore, SqlReactionStore};
pub use role_store::{RoleStore, SqlRoleStore};
pub use scheme_store::{SchemeStore, SqlSchemeStore};
pub use session_store::{SessionStore, SqlSessionStore};
pub use status_store::{SqlStatusStore, StatusStore};
pub use team_store::{SqlTeamStore, TeamStore};
pub use user_store::{SqlUserStore, UserStore};
pub use user_terms_of_service_store::{SqlUserTermsOfServiceStore, UserTermsOfServiceStore};

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// The set of stores, sharing one connection pool.
///
/// Go's `SqlStore` is a single object exposing one accessor per store, and callers reach them as
/// `store.Session()`, `store.User()`. This mirrors that shape closely enough to keep the porting
/// map obvious, while the stores stay independently constructible for tests.
#[derive(Debug, Clone)]
pub struct SqlStore {
    channel: SqlChannelStore,
    emoji: SqlEmojiStore,
    file_info: SqlFileInfoStore,
    post: SqlPostStore,
    reaction: SqlReactionStore,
    preference: SqlPreferenceStore,
    role: SqlRoleStore,
    scheme: SqlSchemeStore,
    session: SqlSessionStore,
    status: SqlStatusStore,
    team: SqlTeamStore,
    user: SqlUserStore,
    user_terms_of_service: SqlUserTermsOfServiceStore,
}

impl SqlStore {
    /// Connect and build the store set.
    ///
    /// The pool is deliberately small by default. The Go server is sizing its own pool against
    /// the same Postgres, and the migration's failure mode is exhausting connections between two
    /// servers that each believe they are alone.
    pub async fn connect(database_url: &str, max_connections: u32) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(database_url)
            .await
            .map_err(|source| StoreError::Db {
                context: "failed to connect to Postgres".to_owned(),
                source,
            })?;

        Ok(Self::from_pool(pool))
    }

    /// Build the store set over an existing pool.
    pub fn from_pool(pool: PgPool) -> Self {
        Self {
            channel: SqlChannelStore::new(pool.clone()),
            emoji: SqlEmojiStore::new(pool.clone()),
            file_info: SqlFileInfoStore::new(pool.clone()),
            post: SqlPostStore::new(pool.clone()),
            reaction: SqlReactionStore::new(pool.clone()),
            preference: SqlPreferenceStore::new(pool.clone()),
            role: SqlRoleStore::new(pool.clone()),
            scheme: SqlSchemeStore::new(pool.clone()),
            session: SqlSessionStore::new(pool.clone()),
            status: SqlStatusStore::new(pool.clone()),
            team: SqlTeamStore::new(pool.clone()),
            user_terms_of_service: SqlUserTermsOfServiceStore::new(pool.clone()),
            user: SqlUserStore::new(pool),
        }
    }

    /// Port of `store.Store.Channel()`.
    pub fn channel(&self) -> &SqlChannelStore {
        &self.channel
    }

    /// Port of `store.Store.Session()`.
    pub fn session(&self) -> &SqlSessionStore {
        &self.session
    }

    /// Port of `store.Store.Emoji()`.
    pub fn emoji(&self) -> &SqlEmojiStore {
        &self.emoji
    }

    /// Port of `store.Store.FileInfo()`.
    pub fn file_info(&self) -> &SqlFileInfoStore {
        &self.file_info
    }

    /// Port of `store.Store.Post()` — and, folded in, `PostPriority()` and
    /// `PostAcknowledgement()`. See [`post_store`].
    pub fn post(&self) -> &SqlPostStore {
        &self.post
    }

    /// Port of `store.Store.Reaction()`.
    pub fn reaction(&self) -> &SqlReactionStore {
        &self.reaction
    }

    /// Port of `store.Store.Preference()`.
    pub fn preference(&self) -> &SqlPreferenceStore {
        &self.preference
    }

    /// Port of `store.Store.Role()`.
    pub fn role(&self) -> &SqlRoleStore {
        &self.role
    }

    /// Port of `store.Store.Scheme()`.
    pub fn scheme(&self) -> &SqlSchemeStore {
        &self.scheme
    }

    /// Port of `store.Store.Status()`.
    pub fn status(&self) -> &SqlStatusStore {
        &self.status
    }

    /// Port of `store.Store.Team()`.
    pub fn team(&self) -> &SqlTeamStore {
        &self.team
    }

    /// Port of `store.Store.User()`.
    pub fn user(&self) -> &SqlUserStore {
        &self.user
    }

    /// Port of `store.Store.UserTermsOfService()`.
    pub fn user_terms_of_service(&self) -> &SqlUserTermsOfServiceStore {
        &self.user_terms_of_service
    }
}
