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

pub mod access_control_policy_store;
pub mod audit_store;
pub mod bot_store;
pub mod channel_join_request_store;
pub mod channel_member_history_store;
pub mod channel_store;
pub mod command_store;
pub mod config_store;
pub mod desktop_tokens_store;
pub mod draft_store;
pub mod emoji_store;
pub mod error;
pub mod file_info_store;
/// The one `GroupStore` read the channel-member add path needs.
pub mod group_store;
pub mod group_syncable_store;
pub mod job_store;
pub mod license_store;
pub mod notify_admin_store;
pub mod oauth_store;
pub mod post_acknowledgement_store;
pub mod post_store;
pub mod preference_store;
pub mod product_notices_store;
/// The five CPA reads across `PropertyGroups`, `PropertyFields` and `PropertyValues`.
pub mod property_store;
pub mod reaction_store;
pub mod read_receipt_store;
pub mod role_store;
pub mod scheme_store;
pub mod session_store;
/// The read side of `SidebarCategories` — Go hangs these off `ChannelStore`.
pub mod sidebar_category_store;
pub mod status_store;
pub mod system_store;
pub mod team_store;
pub mod temporary_post_store;
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

pub use access_control_policy_store::{AccessControlPolicyStore, SqlAccessControlPolicyStore};
pub use audit_store::{AUDIT_LIMIT_MAXIMUM, AuditStore, SqlAuditStore};
pub use bot_store::{BotStore, SqlBotStore};
pub use channel_join_request_store::{ChannelJoinRequestStore, SqlChannelJoinRequestStore};
pub use channel_member_history_store::{ChannelMemberHistoryStore, SqlChannelMemberHistoryStore};
pub use channel_store::{ChannelSave, ChannelStore, SqlChannelStore, UnreadsAndMentions};
pub use command_store::{CommandStore, SqlCommandStore};
pub use config_store::{ConfigStore, SqlConfigStore};
pub use desktop_tokens_store::{DesktopTokensStore, SqlDesktopTokensStore};
pub use draft_store::{DraftStore, SqlDraftStore};
pub use emoji_store::{EmojiStore, SqlEmojiStore};
pub use error::StoreError;
pub use file_info_store::{FileInfoStore, SqlFileInfoStore};
pub use group_store::{GroupStore, SqlGroupStore};
pub use group_syncable_store::GroupSyncableStore;
pub use job_store::{JobStore, SqlJobStore};
pub use license_store::{LicenseStore, SqlLicenseStore};
pub use notify_admin_store::{NotifyAdminStore, SqlNotifyAdminStore};
pub use oauth_store::{OAuthStore, SqlOAuthStore};
pub use post_acknowledgement_store::{PostAcknowledgementStore, SqlPostAcknowledgementStore};
pub use post_store::{PostStore, SqlPostStore};
pub use preference_store::{PreferenceStore, SqlPreferenceStore};
pub use product_notices_store::{ProductNoticesStore, SqlProductNoticesStore};
pub use property_store::{PropertyStore, SqlPropertyStore};
pub use reaction_store::{ReactionStore, SqlReactionStore};
pub use read_receipt_store::{ReadReceiptStore, SqlReadReceiptStore};
pub use role_store::{RoleStore, SqlRoleStore};
pub use scheme_store::{SchemeStore, SqlSchemeStore};
pub use session_store::{SessionStore, SqlSessionStore};
pub use sidebar_category_store::{
    SidebarCategoryStore, SidebarCategoryUpdate, SqlSidebarCategoryStore,
};
pub use status_store::{SqlStatusStore, StatusStore};
pub use system_store::{SYSTEM_ACTIVE_LICENSE_ID, SqlSystemStore, SystemStore};
pub use team_store::{SqlTeamStore, TeamStore};
pub use temporary_post_store::{SqlTemporaryPostStore, TemporaryPostStore};
pub use terms_of_service_store::{SqlTermsOfServiceStore, TermsOfServiceStore};
pub use thread_store::{SqlThreadStore, ThreadStore};
pub use token_store::{SqlTokenStore, TokenStore};
pub use upload_session_store::{SqlUploadSessionStore, UploadSessionStore};
pub use user_access_token_store::{SqlUserAccessTokenStore, UserAccessTokenStore};
pub use user_store::{SqlUserStore, UserStore};
pub use user_terms_of_service_store::{SqlUserTermsOfServiceStore, UserTermsOfServiceStore};
pub use view_store::{SqlViewStore, ViewStore};
pub use webhook_store::{SqlWebhookStore, WebhookStore};

use mm_model::integrity::{IntegrityCheckResult, OrphanedRecord, RelationalIntegrityCheckData};
use mm_model::system::AppliedMigration;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, Row};

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
    notify_admin: SqlNotifyAdminStore,
    desktop_tokens: SqlDesktopTokensStore,
    read_receipt: SqlReadReceiptStore,
    temporary_post: SqlTemporaryPostStore,
    upload_session: SqlUploadSessionStore,
    job: SqlJobStore,
    access_control_policy: SqlAccessControlPolicyStore,
    post_acknowledgement: SqlPostAcknowledgementStore,
    product_notices: SqlProductNoticesStore,
    license: SqlLicenseStore,
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
            notify_admin: SqlNotifyAdminStore::new(pool.clone()),
            desktop_tokens: SqlDesktopTokensStore::new(pool.clone()),
            read_receipt: SqlReadReceiptStore::new(pool.clone()),
            temporary_post: SqlTemporaryPostStore::new(pool.clone()),
            upload_session: SqlUploadSessionStore::new(pool.clone()),
            job: SqlJobStore::new(pool.clone()),
            access_control_policy: SqlAccessControlPolicyStore::new(pool.clone()),
            post_acknowledgement: SqlPostAcknowledgementStore::new(pool.clone()),
            product_notices: SqlProductNoticesStore::new(pool.clone()),
            license: SqlLicenseStore::new(pool.clone()),
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

    /// Port of `SqlStore.GetDbVersion` (sqlstore/store.go:422): `SHOW server_version_num` or
    /// `SHOW server_version`, verbatim — `16.4 (Debian 16.4-1.pgdg120+1)` for the latter, which
    /// the one caller trims at its first space.
    #[tracing::instrument(skip(self), fields(version))]
    pub async fn get_db_version(&self, numerical: bool) -> Result<String, StoreError> {
        let statement = if numerical {
            "SHOW server_version_num"
        } else {
            "SHOW server_version"
        };
        let version: String = sqlx::query_scalar(statement)
            .fetch_one(&self.pool)
            .await
            .map_err(|source| StoreError::Db {
                context: "failed to read the database version".to_owned(),
                source,
            })?;
        tracing::Span::current().record("version", &version);
        Ok(version)
    }

    /// Port of `SqlStore.TotalMasterDbConnections` (sqlstore/store.go:555) —
    /// `sql.DBStats.OpenConnections`, which is every connection the pool holds, idle or in use.
    /// sqlx's `size()` is the same figure for this pool.
    ///
    /// **A per-process number, never a shared one.** Each server counts its own pool, so the
    /// two disagree by design and `GET /api/v4/analytics/old` cannot be compared on this row.
    pub fn total_master_db_connections(&self) -> i64 {
        i64::from(self.pool.size())
    }

    /// Port of `SqlStore.TotalReadDbConnections` (sqlstore/store.go:601): the sum over the
    /// read replicas, and **0 when `SqlSettings.DataSourceReplicas` is empty** — which is the
    /// only configuration this store has; it opens one pool against one data source and reads
    /// no replica list. The constant is Go's own answer for that configuration.
    pub fn total_read_db_connections(&self) -> i64 {
        0
    }

    /// Port of `SqlStore.RecycleDBConnections` (sqlstore/store.go), as far as sqlx allows.
    ///
    /// Go sets `SetConnMaxLifetime(10s)` on the live pool — permanently, since nothing ever
    /// restores it — so every connection is closed within ten seconds of being returned and a
    /// fresh one dialled in its place. sqlx fixes a pool's lifetime at construction, so the
    /// nearest honest equivalent is to close what is idle **now**: each idle connection is taken
    /// with `try_acquire` (never waiting, never dialling), detached from the pool and closed,
    /// and the pool dials a replacement on demand. A connection checked out by a request in
    /// flight is not touched; Go's would be closed when that request returns it. Nothing about
    /// the pool's configuration changes.
    #[tracing::instrument(skip_all, fields(idle, closed))]
    pub async fn recycle_db_connections(&self) {
        let idle = self.pool.num_idle();
        tracing::Span::current().record("idle", idle);
        let mut closed = 0usize;
        for _ in 0..idle {
            let Some(connection) = self.pool.try_acquire() else {
                break;
            };
            match connection.detach().close().await {
                Ok(()) => closed += 1,
                Err(err) => tracing::warn!(error = %err, "closing a recycled connection failed"),
            }
        }
        tracing::Span::current().record("closed", closed);
    }

    /// Port of `SqlStore.CheckIntegrity` (sqlstore/store.go:1009) and the whole of
    /// `sqlstore/integrity.go`: the forty-one relational checks, in Go's order, each answered
    /// as one [`IntegrityCheckResult`].
    ///
    /// Go streams them over a channel from one goroutine, so the order is the order of the
    /// calls in `CheckRelationalIntegrity` and this returns them the same way. Each check is
    /// `getOrphanedRecords`'s statement built from a [`RelationalCheck`] — the child rows whose
    /// parent id names no parent, `ORDER BY` the parent id and nothing else, so two orphans of
    /// one parent come back in whatever order the planner chose; a comparison across servers
    /// sorts within a parent before it compares. A statement that fails is a result with no
    /// data and the error, logged as Go logs it, and the run continues.
    ///
    /// `checkTeamsChannelsIntegrity` is the one composite: two statements (message channels
    /// with a team id, then direct and group channels whose team id is non-empty) whose records
    /// are concatenated under the first's header. Go reads the second's `Data` with an unchecked
    /// type assertion, so its failing would panic the server; here it is the failure result,
    /// which is the one thing about this function that cannot be compared.
    #[tracing::instrument(skip_all, fields(checks, failed))]
    pub async fn check_integrity(&self) -> Vec<IntegrityCheckResult> {
        let mut results = Vec::with_capacity(RELATIONAL_CHECKS.len());
        for check in RELATIONAL_CHECKS {
            let result = match check {
                Check::One(config) => self.check_parent_child_integrity(config).await,
                Check::TeamsChannels(first, second) => {
                    let mut first = self.check_parent_child_integrity(first).await;
                    let second = self.check_parent_child_integrity(second).await;
                    match (first.data.as_mut(), second.data) {
                        (Some(data), Some(more)) => data.records.extend(more.records),
                        (Some(_), None) => {
                            first = IntegrityCheckResult {
                                data: None,
                                err: second.err,
                            }
                        }
                        (None, _) => {}
                    }
                    first
                }
            };
            results.push(result);
        }
        tracing::Span::current().record("checks", results.len());
        tracing::Span::current()
            .record("failed", results.iter().filter(|r| r.err.is_some()).count());
        results
    }

    /// Port of `checkParentChildIntegrity` (sqlstore/integrity.go:66): the statement, run on
    /// the master as Go does, and its two outcomes.
    async fn check_parent_child_integrity(&self, config: &RelationalCheck) -> IntegrityCheckResult {
        let sql = config.orphaned_records_sql();
        let rows = match sqlx::query(&sql).fetch_all(&self.pool).await {
            Ok(rows) => rows,
            Err(err) => {
                tracing::error!(error = %err, statement = %sql, "Error while getting orphaned records");
                return IntegrityCheckResult {
                    data: None,
                    err: Some(err.to_string()),
                };
            }
        };
        let has_child = !config.child_id_attr.is_empty();
        let mut records = Vec::with_capacity(rows.len());
        for row in &rows {
            let parent_id = match row.try_get::<Option<String>, _>(0) {
                Ok(id) => id,
                Err(err) => {
                    return IntegrityCheckResult {
                        data: None,
                        err: Some(err.to_string()),
                    };
                }
            };
            let child_id = if has_child {
                match row.try_get::<Option<String>, _>(1) {
                    Ok(id) => id,
                    Err(err) => {
                        return IntegrityCheckResult {
                            data: None,
                            err: Some(err.to_string()),
                        };
                    }
                }
            } else {
                None
            };
            records.push(OrphanedRecord {
                parent_id,
                child_id,
            });
        }
        IntegrityCheckResult {
            data: Some(RelationalIntegrityCheckData {
                parent_name: config.parent_name.to_owned(),
                child_name: config.child_name.to_owned(),
                parent_id_attr: config.parent_id_attr.to_owned(),
                child_id_attr: config.child_id_attr.to_owned(),
                records,
            }),
            err: None,
        }
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

    /// Port of `store.Store.NotifyAdmin()`.
    pub fn notify_admin(&self) -> &SqlNotifyAdminStore {
        &self.notify_admin
    }

    /// Port of `store.Store.DesktopTokens()`.
    pub fn desktop_tokens(&self) -> &SqlDesktopTokensStore {
        &self.desktop_tokens
    }

    /// Port of `store.Store.ReadReceipt()`.
    pub fn read_receipt(&self) -> &SqlReadReceiptStore {
        &self.read_receipt
    }

    /// Port of `store.Store.TemporaryPost()`.
    pub fn temporary_post(&self) -> &SqlTemporaryPostStore {
        &self.temporary_post
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

    /// Port of `store.Store.AccessControlPolicy()`.
    pub fn access_control_policy(&self) -> &SqlAccessControlPolicyStore {
        &self.access_control_policy
    }

    /// Port of `store.Store.PostAcknowledgement()`.
    pub fn post_acknowledgement(&self) -> &SqlPostAcknowledgementStore {
        &self.post_acknowledgement
    }

    /// Port of `store.Store.ProductNotices()`.
    pub fn product_notices(&self) -> &SqlProductNoticesStore {
        &self.product_notices
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

    /// Port of `store.Store.License()`.
    pub fn license(&self) -> &SqlLicenseStore {
        &self.license
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

/// One row of `integrity.go`'s `relationalCheckConfig` — the parent table, the child table, the
/// child's column that names the parent, and the child's own id column (`""` for a table with
/// no single-column id, whose records then carry `child_id: null`).
#[derive(Debug)]
pub struct RelationalCheck {
    parent_name: &'static str,
    parent_id_attr: &'static str,
    child_name: &'static str,
    child_id_attr: &'static str,
    /// `canParentIdBeEmpty`: an empty parent id is "no parent", not an orphan, so the
    /// statement excludes it.
    can_parent_id_be_empty: bool,
    /// `filter`, already rendered — Go's two are `sq.Eq`/`sq.NotEq` over the channel type.
    filter: Option<&'static str>,
}

impl RelationalCheck {
    /// Port of `getOrphanedRecords`'s statement (sqlstore/integrity.go:23), column for column
    /// and clause for clause in squirrel's order. The identifiers are these constants and never
    /// a caller's, which is what makes the formatting safe.
    fn orphaned_records_sql(&self) -> String {
        let RelationalCheck {
            parent_name,
            parent_id_attr,
            child_name,
            child_id_attr,
            can_parent_id_be_empty,
            filter,
        } = self;
        let mut sql = format!("SELECT CT.{parent_id_attr} AS ParentId");
        if !child_id_attr.is_empty() {
            sql.push_str(&format!(", CT.{child_id_attr} AS ChildId"));
        }
        sql.push_str(&format!(
            " FROM {child_name} AS CT WHERE NOT EXISTS (SELECT TRUE FROM {parent_name} AS PT WHERE PT.id = CT.{parent_id_attr})"
        ));
        if *can_parent_id_be_empty {
            sql.push_str(&format!(" AND CT.{parent_id_attr} <> ''"));
        }
        if let Some(filter) = filter {
            sql.push_str(&format!(" AND {filter}"));
        }
        sql.push_str(&format!(" ORDER BY CT.{parent_id_attr}"));
        sql
    }
}

/// A check as `CheckRelationalIntegrity` runs it: one statement, or the teams-channels pair.
#[derive(Debug)]
enum Check {
    One(RelationalCheck),
    TeamsChannels(RelationalCheck, RelationalCheck),
}

const fn check(
    parent_name: &'static str,
    parent_id_attr: &'static str,
    child_name: &'static str,
    child_id_attr: &'static str,
    can_parent_id_be_empty: bool,
) -> RelationalCheck {
    RelationalCheck {
        parent_name,
        parent_id_attr,
        child_name,
        child_id_attr,
        can_parent_id_be_empty,
        filter: None,
    }
}

/// `sq.NotEq{"CT.Type": []model.ChannelType{Direct, Group}}`.
const NOT_DIRECT_OR_GROUP: &str = "CT.Type NOT IN ('D','G')";
/// `sq.Eq{"CT.Type": []model.ChannelType{Direct, Group}}`.
const DIRECT_OR_GROUP: &str = "CT.Type IN ('D','G')";

/// The forty-one checks of `CheckRelationalIntegrity` (sqlstore/integrity.go:518), in the order
/// its seven groups send them: channels, commands, posts, schemes, sessions, teams, users.
const RELATIONAL_CHECKS: &[Check] = &[
    // checkChannelsIntegrity
    Check::One(check(
        "Channels",
        "ChannelId",
        "CommandWebhooks",
        "Id",
        false,
    )),
    Check::One(check(
        "Channels",
        "ChannelId",
        "ChannelMemberHistory",
        "",
        false,
    )),
    Check::One(check("Channels", "ChannelId", "ChannelMembers", "", false)),
    Check::One(check(
        "Channels",
        "ChannelId",
        "IncomingWebhooks",
        "Id",
        false,
    )),
    Check::One(check(
        "Channels",
        "ChannelId",
        "OutgoingWebhooks",
        "Id",
        false,
    )),
    Check::One(check("Channels", "ChannelId", "Posts", "Id", false)),
    Check::One(check("Channels", "ChannelId", "FileInfo", "Id", false)),
    // checkCommandsIntegrity
    Check::One(check(
        "Commands",
        "CommandId",
        "CommandWebhooks",
        "Id",
        false,
    )),
    // checkPostsIntegrity
    Check::One(check("Posts", "PostId", "FileInfo", "Id", false)),
    Check::One(check("Posts", "RootId", "Posts", "Id", true)),
    Check::One(check("Posts", "PostId", "Reactions", "", false)),
    Check::One(check("Teams", "ThreadTeamId", "Threads", "PostId", false)),
    // checkSchemesIntegrity
    Check::One(check("Schemes", "SchemeId", "Channels", "Id", true)),
    Check::One(check("Schemes", "SchemeId", "Teams", "Id", true)),
    // checkSessionsIntegrity
    Check::One(check("Sessions", "SessionId", "Audits", "Id", true)),
    // checkTeamsIntegrity
    Check::TeamsChannels(
        RelationalCheck {
            filter: Some(NOT_DIRECT_OR_GROUP),
            ..check("Teams", "TeamId", "Channels", "Id", false)
        },
        RelationalCheck {
            filter: Some(DIRECT_OR_GROUP),
            ..check("Teams", "TeamId", "Channels", "Id", true)
        },
    ),
    Check::One(check("Teams", "TeamId", "Commands", "Id", false)),
    Check::One(check("Teams", "TeamId", "IncomingWebhooks", "Id", false)),
    Check::One(check("Teams", "TeamId", "OutgoingWebhooks", "Id", false)),
    Check::One(check("Teams", "TeamId", "TeamMembers", "", false)),
    // checkUsersIntegrity
    Check::One(check("Users", "UserId", "Audits", "Id", true)),
    Check::One(check("Users", "UserId", "CommandWebhooks", "Id", false)),
    Check::One(check("Users", "UserId", "ChannelMemberHistory", "", false)),
    Check::One(check("Users", "UserId", "ChannelMembers", "", false)),
    Check::One(check("Users", "CreatorId", "Channels", "Id", true)),
    Check::One(check("Users", "CreatorId", "Commands", "Id", false)),
    Check::One(check("Users", "UserId", "Compliances", "Id", false)),
    Check::One(check("Users", "CreatorId", "Emoji", "Id", false)),
    Check::One(check("Users", "CreatorId", "FileInfo", "Id", false)),
    Check::One(check("Users", "UserId", "IncomingWebhooks", "Id", false)),
    Check::One(check("Users", "UserId", "OAuthAccessData", "Token", false)),
    Check::One(check("Users", "CreatorId", "OAuthApps", "Id", false)),
    Check::One(check("Users", "UserId", "OAuthAuthData", "Code", false)),
    Check::One(check("Users", "CreatorId", "OutgoingWebhooks", "Id", false)),
    Check::One(check("Users", "UserId", "Posts", "Id", false)),
    Check::One(check("Users", "UserId", "Preferences", "", false)),
    Check::One(check("Users", "UserId", "Reactions", "", false)),
    Check::One(check("Users", "UserId", "Sessions", "Id", false)),
    Check::One(check("Users", "UserId", "Status", "", false)),
    Check::One(check("Users", "UserId", "TeamMembers", "", false)),
    Check::One(check("Users", "UserId", "UserAccessTokens", "Id", false)),
];

#[cfg(test)]
mod integrity_tests {
    use super::*;

    /// The statement squirrel renders for `checkChannelsCommandWebhooksIntegrity`, and the
    /// three optional clauses in the positions Go adds them.
    #[test]
    fn the_orphan_statement_is_squirrels() {
        let plain = check("Channels", "ChannelId", "CommandWebhooks", "Id", false);
        assert_eq!(
            plain.orphaned_records_sql(),
            "SELECT CT.ChannelId AS ParentId, CT.Id AS ChildId FROM CommandWebhooks AS CT \
             WHERE NOT EXISTS (SELECT TRUE FROM Channels AS PT WHERE PT.id = CT.ChannelId) \
             ORDER BY CT.ChannelId"
        );
        let no_child = check("Channels", "ChannelId", "ChannelMembers", "", false);
        assert_eq!(
            no_child.orphaned_records_sql(),
            "SELECT CT.ChannelId AS ParentId FROM ChannelMembers AS CT \
             WHERE NOT EXISTS (SELECT TRUE FROM Channels AS PT WHERE PT.id = CT.ChannelId) \
             ORDER BY CT.ChannelId"
        );
        let optional = RelationalCheck {
            filter: Some(DIRECT_OR_GROUP),
            ..check("Teams", "TeamId", "Channels", "Id", true)
        };
        assert_eq!(
            optional.orphaned_records_sql(),
            "SELECT CT.TeamId AS ParentId, CT.Id AS ChildId FROM Channels AS CT \
             WHERE NOT EXISTS (SELECT TRUE FROM Teams AS PT WHERE PT.id = CT.TeamId) \
             AND CT.TeamId <> '' AND CT.Type IN ('D','G') ORDER BY CT.TeamId"
        );
    }

    /// Forty-one results, in Go's order: the seven groups' first and last members, and the
    /// composite in the teams group.
    #[test]
    fn the_checks_are_the_forty_one_of_integrity_go() {
        assert_eq!(RELATIONAL_CHECKS.len(), 41);
        let name = |i: usize| match &RELATIONAL_CHECKS[i] {
            Check::One(c) => (c.parent_name, c.child_name, c.parent_id_attr),
            Check::TeamsChannels(c, _) => (c.parent_name, c.child_name, c.parent_id_attr),
        };
        assert_eq!(name(0), ("Channels", "CommandWebhooks", "ChannelId"));
        assert_eq!(name(7), ("Commands", "CommandWebhooks", "CommandId"));
        assert_eq!(name(11), ("Teams", "Threads", "ThreadTeamId"));
        assert_eq!(name(15), ("Teams", "Channels", "TeamId"));
        assert!(matches!(RELATIONAL_CHECKS[15], Check::TeamsChannels(..)));
        assert_eq!(name(20), ("Users", "Audits", "UserId"));
        assert_eq!(name(40), ("Users", "UserAccessTokens", "UserId"));
    }
}
