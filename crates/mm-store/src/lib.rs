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

pub mod audit_store;
pub mod bot_store;
pub mod channel_join_request_store;
pub mod channel_member_history_store;
pub mod channel_store;
pub mod command_store;
pub mod config_store;
pub mod draft_store;
pub mod emoji_store;
pub mod error;
pub mod file_info_store;
/// The one `GroupStore` read the channel-member add path needs.
pub mod group_store;
pub mod job_store;
pub mod oauth_store;
pub mod post_store;
pub mod preference_store;
/// The five CPA reads across `PropertyGroups`, `PropertyFields` and `PropertyValues`.
pub mod property_store;
pub mod reaction_store;
pub mod role_store;
pub mod scheme_store;
pub mod session_store;
/// The read side of `SidebarCategories` — Go hangs these off `ChannelStore`.
pub mod sidebar_category_store;
pub mod status_store;
pub mod system_store;
pub mod team_store;
pub mod terms_of_service_store;
pub mod thread_store;
pub mod token_store;
/// Port of `SqlUploadSessionStore` — the two reads.
pub mod upload_session_store;
pub mod user_access_token_store;
pub mod user_store;
pub mod user_terms_of_service_store;
/// Port of `SqlViewStore` — the whole of the integrated-boards store.
pub mod view_store;
pub mod webhook_store;

pub use audit_store::{AUDIT_LIMIT_MAXIMUM, AuditStore, SqlAuditStore};
pub use bot_store::{BotStore, SqlBotStore};
pub use channel_join_request_store::{ChannelJoinRequestStore, SqlChannelJoinRequestStore};
pub use channel_member_history_store::{ChannelMemberHistoryStore, SqlChannelMemberHistoryStore};
pub use channel_store::{ChannelSave, ChannelStore, SqlChannelStore, UnreadsAndMentions};
pub use command_store::{CommandStore, SqlCommandStore};
pub use config_store::{ConfigStore, SqlConfigStore};
pub use draft_store::{DraftStore, SqlDraftStore};
pub use emoji_store::{EmojiStore, SqlEmojiStore};
pub use error::StoreError;
pub use file_info_store::{FileInfoStore, SqlFileInfoStore};
pub use group_store::{GroupStore, SqlGroupStore};
pub use job_store::{JobStore, SqlJobStore};
pub use oauth_store::{OAuthStore, SqlOAuthStore};
pub use post_store::{PostStore, SqlPostStore};
pub use preference_store::{PreferenceStore, SqlPreferenceStore};
pub use property_store::{PropertyStore, SqlPropertyStore};
pub use reaction_store::{ReactionStore, SqlReactionStore};
pub use role_store::{RoleStore, SqlRoleStore};
pub use scheme_store::{SchemeStore, SqlSchemeStore};
pub use session_store::{SessionStore, SqlSessionStore};
pub use sidebar_category_store::{
    SidebarCategoryStore, SidebarCategoryUpdate, SqlSidebarCategoryStore,
};
pub use status_store::{SqlStatusStore, StatusStore};
pub use system_store::{SYSTEM_ACTIVE_LICENSE_ID, SqlSystemStore, SystemStore};
pub use team_store::{SqlTeamStore, TeamStore};
pub use terms_of_service_store::{SqlTermsOfServiceStore, TermsOfServiceStore};
pub use thread_store::{SqlThreadStore, ThreadStore};
pub use token_store::{SqlTokenStore, TokenStore};
pub use upload_session_store::{SqlUploadSessionStore, UploadSessionStore};
pub use user_access_token_store::{SqlUserAccessTokenStore, UserAccessTokenStore};
pub use user_store::{SqlUserStore, UserStore};
pub use user_terms_of_service_store::{SqlUserTermsOfServiceStore, UserTermsOfServiceStore};
pub use view_store::{SqlViewStore, ViewStore};
pub use webhook_store::{SqlWebhookStore, WebhookStore};

use mm_model::system::AppliedMigration;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// The set of stores, sharing one connection pool.
///
/// Go's `SqlStore` is a single object exposing one accessor per store, and callers reach them as
/// `store.Session()`, `store.User()`. This mirrors that shape closely enough to keep the porting
/// map obvious, while the stores stay independently constructible for tests.
#[derive(Debug, Clone)]
pub struct SqlStore {
    audit: SqlAuditStore,
    bot: SqlBotStore,
    command: SqlCommandStore,
    channel: SqlChannelStore,
    channel_join_request: SqlChannelJoinRequestStore,
    config: SqlConfigStore,
    emoji: SqlEmojiStore,
    draft: SqlDraftStore,
    file_info: SqlFileInfoStore,
    upload_session: SqlUploadSessionStore,
    job: SqlJobStore,
    oauth: SqlOAuthStore,
    post: SqlPostStore,
    reaction: SqlReactionStore,
    terms_of_service: SqlTermsOfServiceStore,
    thread: SqlThreadStore,
    preference: SqlPreferenceStore,
    property: SqlPropertyStore,
    role: SqlRoleStore,
    scheme: SqlSchemeStore,
    session: SqlSessionStore,
    sidebar_category: SqlSidebarCategoryStore,
    status: SqlStatusStore,
    system: SqlSystemStore,
    team: SqlTeamStore,
    token: SqlTokenStore,
    user: SqlUserStore,
    user_access_token: SqlUserAccessTokenStore,
    user_terms_of_service: SqlUserTermsOfServiceStore,
    webhook: SqlWebhookStore,
    channel_member_history: SqlChannelMemberHistoryStore,
    group: SqlGroupStore,
    view: SqlViewStore,
    /// Go's `SqlStore` owns the connections and hands them to each sub-store; a handful of its
    /// methods — [`SqlStore::get_applied_migrations`] is the first ported — query directly rather
    /// than through a sub-store, which is why the pool is held here too. `PgPool` is a handle over
    /// shared internals, so this is a clone of a pointer and not a second pool.
    pool: PgPool,
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
            audit: SqlAuditStore::new(pool.clone()),
            bot: SqlBotStore::new(pool.clone()),
            command: SqlCommandStore::new(pool.clone()),
            channel: SqlChannelStore::new(pool.clone()),
            channel_join_request: SqlChannelJoinRequestStore::new(pool.clone()),
            config: SqlConfigStore::new(pool.clone()),
            emoji: SqlEmojiStore::new(pool.clone()),
            draft: SqlDraftStore::new(pool.clone()),
            file_info: SqlFileInfoStore::new(pool.clone()),
            upload_session: SqlUploadSessionStore::new(pool.clone()),
            job: SqlJobStore::new(pool.clone()),
            oauth: SqlOAuthStore::new(pool.clone()),
            post: SqlPostStore::new(pool.clone()),
            reaction: SqlReactionStore::new(pool.clone()),
            terms_of_service: SqlTermsOfServiceStore::new(pool.clone()),
            thread: SqlThreadStore::new(pool.clone()),
            preference: SqlPreferenceStore::new(pool.clone()),
            property: SqlPropertyStore::new(pool.clone()),
            role: SqlRoleStore::new(pool.clone()),
            scheme: SqlSchemeStore::new(pool.clone()),
            session: SqlSessionStore::new(pool.clone()),
            sidebar_category: SqlSidebarCategoryStore::new(pool.clone()),
            status: SqlStatusStore::new(pool.clone()),
            system: SqlSystemStore::new(pool.clone()),
            team: SqlTeamStore::new(pool.clone()),
            token: SqlTokenStore::new(pool.clone()),
            user_terms_of_service: SqlUserTermsOfServiceStore::new(pool.clone()),
            webhook: SqlWebhookStore::new(pool.clone()),
            user: SqlUserStore::new(pool.clone()),
            user_access_token: SqlUserAccessTokenStore::new(pool.clone()),
            channel_member_history: SqlChannelMemberHistoryStore::new(pool.clone()),
            group: SqlGroupStore::new(pool.clone()),
            view: SqlViewStore::new(pool.clone()),
            pool,
        }
    }

    /// Port of `SqlStore.GetAppliedMigrations` (sqlstore/store.go:1120).
    ///
    /// `SELECT Version, Name FROM db_migrations ORDER BY Version DESC` — newest first, and on
    /// `GetMaster()` rather than the replica, which is Go making sure a migration that has just
    /// finished is visible. There is one pool here, so that distinction has no effect.
    ///
    /// This lives on the store rather than on a sub-store because that is where Go puts it:
    /// `db_migrations` is the migrator's own bookkeeping table and belongs to no entity.
    #[tracing::instrument(skip_all, fields(found))]
    pub async fn get_applied_migrations(&self) -> Result<Vec<AppliedMigration>, StoreError> {
        let rows = sqlx::query_as!(
            AppliedMigration,
            r#"SELECT version AS "version!", name AS "name!" FROM db_migrations ORDER BY version DESC"#
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "unable to select from db_migrations".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        Ok(rows)
    }

    /// Port of `store.Store.Audit()`.
    pub fn audit(&self) -> &SqlAuditStore {
        &self.audit
    }

    /// Port of `store.Store.TermsOfService()`.
    pub fn terms_of_service(&self) -> &SqlTermsOfServiceStore {
        &self.terms_of_service
    }

    /// Port of `store.Store.OAuth()`.
    pub fn oauth(&self) -> &SqlOAuthStore {
        &self.oauth
    }

    /// Port of `store.Store.Webhook()`.
    pub fn webhook(&self) -> &SqlWebhookStore {
        &self.webhook
    }

    /// Port of `store.Store.Channel()`.
    pub fn channel(&self) -> &SqlChannelStore {
        &self.channel
    }

    /// Port of `store.Store.ChannelMemberHistory()`.
    pub fn channel_member_history(&self) -> &SqlChannelMemberHistoryStore {
        &self.channel_member_history
    }

    /// Port of `store.Store.Group()`.
    pub fn group(&self) -> &SqlGroupStore {
        &self.group
    }

    /// The configuration document store.
    ///
    /// Go has no `store.Store.Config()` — its config store is a separate `config.Store` built
    /// before the app store and injected into `Platform`. It is folded in here because it is a
    /// table read against the same pool and splitting it would buy nothing but a second pool.
    pub fn config(&self) -> &SqlConfigStore {
        &self.config
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
    pub fn draft(&self) -> &SqlDraftStore {
        &self.draft
    }

    pub fn file_info(&self) -> &SqlFileInfoStore {
        &self.file_info
    }

    /// Port of `store.Store.UploadSession()`.
    pub fn upload_session(&self) -> &SqlUploadSessionStore {
        &self.upload_session
    }

    /// Port of `store.Store.Job()`.
    pub fn job(&self) -> &SqlJobStore {
        &self.job
    }

    /// Port of `store.Store.Bot()`.
    pub fn bot(&self) -> &SqlBotStore {
        &self.bot
    }

    /// Port of `store.Store.Command()`.
    pub fn command(&self) -> &SqlCommandStore {
        &self.command
    }

    /// Port of `store.Store.UserAccessToken()`.
    pub fn user_access_token(&self) -> &SqlUserAccessTokenStore {
        &self.user_access_token
    }

    /// Port of `store.Store.Post()` — and, folded in, `PostPriority()` and
    /// `PostAcknowledgement()`. See [`post_store`].
    pub fn post(&self) -> &SqlPostStore {
        &self.post
    }

    /// Port of `store.Store.Reaction()`.
    pub fn thread(&self) -> &SqlThreadStore {
        &self.thread
    }

    pub fn reaction(&self) -> &SqlReactionStore {
        &self.reaction
    }

    /// Port of `store.Store.Preference()`.
    pub fn preference(&self) -> &SqlPreferenceStore {
        &self.preference
    }

    /// The `PropertyGroup()`, `PropertyField()` and `PropertyValue()` reads, which Go exposes as
    /// three accessors over one table family.
    pub fn property(&self) -> &SqlPropertyStore {
        &self.property
    }

    /// Port of `store.Store.Role()`.
    pub fn role(&self) -> &SqlRoleStore {
        &self.role
    }

    /// Port of `store.Store.Scheme()`.
    pub fn scheme(&self) -> &SqlSchemeStore {
        &self.scheme
    }

    /// The sidebar-category reads. Go reaches them through `store.Store.Channel()`; they are
    /// their own store here so the sidebar routes migrate independently of the channel ones.
    pub fn sidebar_category(&self) -> &SqlSidebarCategoryStore {
        &self.sidebar_category
    }

    /// Port of `store.Store.Status()`.
    pub fn status(&self) -> &SqlStatusStore {
        &self.status
    }

    /// Port of `store.Store.System()`.
    pub fn system(&self) -> &SqlSystemStore {
        &self.system
    }

    /// Port of `store.Store.Team()`.
    pub fn team(&self) -> &SqlTeamStore {
        &self.team
    }

    /// Port of `store.Store.Token()` — the one-shot `Tokens` table, not `UserAccessTokens`.
    pub fn token(&self) -> &SqlTokenStore {
        &self.token
    }

    /// Port of `store.Store.User()`.
    pub fn user(&self) -> &SqlUserStore {
        &self.user
    }

    /// Port of `store.Store.UserTermsOfService()`.
    pub fn user_terms_of_service(&self) -> &SqlUserTermsOfServiceStore {
        &self.user_terms_of_service
    }

    /// Port of `store.Store.View()`.
    pub fn view(&self) -> &SqlViewStore {
        &self.view
    }

    /// Port of `store.Store.ChannelJoinRequest()`.
    pub fn channel_join_request(&self) -> &SqlChannelJoinRequestStore {
        &self.channel_join_request
    }
}
