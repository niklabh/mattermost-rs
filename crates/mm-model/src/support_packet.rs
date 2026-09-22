//! Port of `model/support_packet.go` — the diagnostics bundle customers send to Mattermost staff.
//!
//! # Almost every tag here is `yaml:`, and three are `json:`
//!
//! The packet is a zip of YAML documents, so the `yaml:` keys are the wire format. The exceptions
//! are [`SupportPacketConfig`] and [`SupportPacketPluginList`], which carry **`json:`** tags
//! because those two files are written as JSON — a distinction easy to lose when porting.
//!
//! # Go's anonymous inline structs become named ones
//!
//! `SupportPacketDiagnostics` nests eleven anonymous structs. Rust has no anonymous struct types,
//! so each becomes a named one; the YAML nesting and key names are unchanged.

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::feature_flags::FeatureFlags;
use crate::goyaml::{MapBuilder, Node};
use crate::job::Job;
use crate::manifest::Manifest;
use crate::role::Role;
use crate::scheme::Scheme;
use crate::utils::StringMap;

/// Port of `model.CurrentSupportPacketVersion` (support_packet.go:8).
pub const CURRENT_SUPPORT_PACKET_VERSION: i64 = 2;
/// Port of `model.SupportPacketErrorFile` (support_packet.go:9) — errors gathered while building
/// the packet are written here rather than failing the request.
pub const SUPPORT_PACKET_ERROR_FILE: &str = "warning.txt";

/// The `license` block of [`SupportPacketDiagnostics`].
///
/// Deliberately **not** the licence itself: company, seat count and SKU only, plus three flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SupportPacketLicense {
    pub company: String,
    pub users: i64,
    pub sku_short_name: String,
    /// `omitempty`.
    pub is_trial: bool,
    /// `yaml:"is_gov_sku"` — the field is `IsGovSKU`.
    pub is_gov_sku: bool,
    pub is_non_production: bool,
}

/// The `server` block: machine, capacity, process lifecycle and software.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SupportPacketServer {
    pub os: String,
    pub architecture: String,
    pub hostname: String,
    pub installation_type: String,

    pub cpu_cores: i64,
    /// Megabytes. A `uint64` in Go.
    pub total_memory_mb: u64,
    /// `omitempty` — present only inside a container.
    pub container_cpu_limit: f64,
    pub container_memory_limit_mb: u64,

    pub process_id: i64,
    /// A real `time.Time`, not epoch milliseconds — YAML renders it as RFC 3339.
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// `omitempty`.
    pub host_started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub open_file_descriptors: i64,
    pub max_file_descriptors: i64,

    pub version: String,
    pub build_hash: String,
    pub go_version: String,
}

/// The `config` block. **One field, and its key is `store_type` while the field is `Source`.**
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SupportPacketConfigSource {
    /// `yaml:"store_type"`.
    pub source: String,
}

/// The `database` block.
///
/// The first twenty fields are always written; the eleven `Option`s are Postgres-only statistics
/// with `omitempty`, so their absence means "not collected", not "zero".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SupportPacketDatabase {
    pub type_: String,
    pub version: String,
    pub schema_version: String,
    pub master_connections: i64,
    pub replica_connections: i64,
    pub search_connections: i64,
    pub master_connections_in_use: i64,
    pub master_connections_idle: i64,
    pub master_pool_wait_count: i64,
    pub master_pool_wait_duration_ms: i64,
    pub master_connections_closed_max_idle: i64,
    pub master_connections_closed_max_lifetime: i64,
    pub replica_connections_in_use: i64,
    pub replica_connections_idle: i64,
    pub replica_pool_wait_count: i64,
    pub replica_pool_wait_duration_ms: i64,
    pub replica_connections_closed_max_idle: i64,
    pub replica_connections_closed_max_lifetime: i64,

    pub cache_hit_ratio: Option<f64>,
    pub deadlocks: Option<i64>,
    pub temp_files: Option<i64>,
    pub temp_bytes_mb: Option<f64>,
    pub rollbacks: Option<i64>,
    pub idle_in_transaction_count: Option<i64>,
    pub longest_query_duration_seconds: Option<f64>,
    pub waiting_for_lock_count: Option<i64>,
    pub posts_dead_tuples: Option<i64>,
    pub posts_last_autovacuum: Option<chrono::DateTime<chrono::Utc>>,
}

/// The `file_store` block. **`Status` is keyed `file_status` and `Driver` is keyed
/// `file_driver`** — the only two fields whose keys carry a prefix their names do not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SupportPacketFileStore {
    /// `yaml:"file_status"`.
    pub status: String,
    pub error: String,
    /// `yaml:"file_driver"`.
    pub driver: String,
    pub filesystem_type: String,
    pub total_mb: u64,
    pub available_mb: u64,
}

/// The `websocket` block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SupportPacketWebsocket {
    pub connections: i64,
}

/// The `cluster` block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SupportPacketCluster {
    pub id: String,
    pub number_of_nodes: i64,
}

/// One `status` / `error` pair, used for e-mail and push under `notifications`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SupportPacketStatusAndError {
    pub status: String,
    /// `omitempty`.
    pub error: String,
}

/// The `notifications` block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SupportPacketNotifications {
    pub email: SupportPacketStatusAndError,
    pub push: SupportPacketStatusAndError,
}

/// The `ldap` block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SupportPacketLdap {
    pub status: String,
    pub error: String,
    pub server_name: String,
    pub server_version: String,
}

/// The `saml` block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SupportPacketSaml {
    pub provider_type: String,
    pub status: String,
    pub error: String,
}

/// The `elastic` block — **keyed `elastic`, not `elastic_search`**.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SupportPacketElasticSearch {
    pub status: String,
    pub backend: String,
    pub server_version: String,
    pub server_plugins: Vec<String>,
    pub error: String,
}

/// Port of `model.OAuthProviderStatus` (support_packet.go:140) — `ok` / `fail` / `disabled`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OAuthProviderStatus {
    pub status: String,
    pub error: String,
}

/// Port of `model.OAuthProviders` (support_packet.go:146).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OAuthProviders {
    pub gitlab: OAuthProviderStatus,
    pub google: OAuthProviderStatus,
    pub office365: OAuthProviderStatus,
    /// `yaml:"openid"` — the field is `OpenID`.
    pub openid: OAuthProviderStatus,
}

/// Port of `model.SupportPacketDiagnostics` (support_packet.go:13).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SupportPacketDiagnostics {
    /// [`CURRENT_SUPPORT_PACKET_VERSION`] when written by this server.
    pub version: i64,
    pub license: SupportPacketLicense,
    pub server: SupportPacketServer,
    pub config: SupportPacketConfigSource,
    pub database: SupportPacketDatabase,
    pub file_store: SupportPacketFileStore,
    pub websocket: SupportPacketWebsocket,
    pub cluster: SupportPacketCluster,
    pub notifications: SupportPacketNotifications,
    pub ldap: SupportPacketLdap,
    pub saml: SupportPacketSaml,
    /// `yaml:"elastic"`.
    pub elastic_search: SupportPacketElasticSearch,
    pub oauth_providers: OAuthProviders,
}

/// Port of `model.SupportPacketStats` (support_packet.go:154) — the instance's counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SupportPacketStats {
    pub registered_users: i64,
    pub active_users: i64,
    pub daily_active_users: i64,
    pub monthly_active_users: i64,
    pub deactivated_users: i64,
    pub guests: i64,
    pub single_channel_guests: i64,
    pub bot_accounts: i64,
    pub posts: i64,
    pub channels: i64,
    pub teams: i64,
    pub slash_commands: i64,
    pub incoming_webhooks: i64,
    pub outgoing_webhooks: i64,
}

/// Port of `model.SupportPacketJobList` (support_packet.go:173) — the latest enterprise job runs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SupportPacketJobList {
    pub ldap_sync_jobs: Vec<Job>,
    pub data_retention_jobs: Vec<Job>,
    pub message_export_jobs: Vec<Job>,
    pub elastic_post_indexing_jobs: Vec<Job>,
    pub elastic_post_aggregation_jobs: Vec<Job>,
    pub migration_jobs: Vec<Job>,
}

/// Port of `model.SupportPacketPermissionInfo` (support_packet.go:184).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SupportPacketPermissionInfo {
    pub roles: Vec<Role>,
    pub schemes: Vec<Scheme>,
}

/// Port of `model.SupportPacketConfig` (support_packet.go:191).
///
/// **JSON, not YAML**, and the embedded `*Config` is inlined — so the file is the ordinary config
/// document with its `FeatureFlags` moved to the **end**: Go's outer field dominates the embedded
/// one, which drops out of its own position. Build it with [`SupportPacketConfig::new`], which
/// takes the flags out of the config; flattening a config that still holds them writes the key
/// twice.
///
/// One of only two types in this file that reach a JSON codec, and therefore one of only two that
/// carry serde derives. `config` is `Box`ed because `Config` is ~1,300 fields wide and this type
/// is passed by value.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SupportPacketConfig {
    #[serde(flatten)]
    pub config: Box<Config>,

    /// `json:"FeatureFlags"`.
    #[serde(rename = "FeatureFlags")]
    pub feature_flags: FeatureFlags,
}

impl SupportPacketConfig {
    /// `model.SupportPacketConfig{Config: config, FeatureFlags: *config.FeatureFlags}`
    /// (platform/support_packet.go:538), with the flags moved out of the embedded config.
    pub fn new(mut config: Config) -> Self {
        let feature_flags = config.feature_flags.take().unwrap_or_default();
        SupportPacketConfig {
            config: Box::new(config),
            feature_flags,
        }
    }
}

/// Port of `model.SupportPacketPluginList` (support_packet.go:198). **JSON**, keys `enabled` and
/// `disabled`.
///
/// Both lists are `Option`: `getPluginsFile` starts from a zero value and only `append`s, so a
/// server with no running plugin writes `"enabled": null`, not `[]` — the shape every Team
/// Edition stack's packet has (`fixtures/behaviour_goyaml.json`, `plugin_list`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SupportPacketPluginList {
    #[serde(rename = "enabled")]
    pub enabled: Option<Vec<Manifest>>,

    #[serde(rename = "disabled")]
    pub disabled: Option<Vec<Manifest>>,
}

/// Port of `model.SupportPacketDatabaseSchema` (support_packet.go:205).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SupportPacketDatabaseSchema {
    pub database_collation: String,
    pub database_encoding: String,
    pub tables: Vec<DatabaseTable>,
}

/// Port of `model.DatabaseTable` (support_packet.go:212).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DatabaseTable {
    pub name: String,
    pub collation: String,
    pub options: StringMap,
    pub columns: Vec<DatabaseColumn>,
    pub indexes: Vec<DatabaseIndex>,
}

/// Port of `model.DatabaseColumn` (support_packet.go:221).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DatabaseColumn {
    pub name: String,
    pub data_type: String,
    /// `omitempty` — absent for types with no length.
    pub max_length: i64,
    /// **Not** `omitempty`, so `is_nullable: false` is always written.
    pub is_nullable: bool,
}

/// Port of `model.DatabaseIndex` (support_packet.go:229).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DatabaseIndex {
    pub name: String,
    /// The full `CREATE INDEX` statement.
    pub definition: String,
}

/// Port of `model.FileData` (support_packet.go:235) — one file inside the packet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileData {
    pub filename: String,
    pub body: Vec<u8>,
}

/// Port of `model.SupportPacketOptions` (support_packet.go:240).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SupportPacketOptions {
    pub include_logs: bool,
    /// Plugin ids whose packet hooks should be called.
    pub plugin_packets: Vec<String>,
    /// **Three-state**, and Go says so explicitly: `None` keeps the server default, `Some(0)`
    /// skips CPU profiling entirely, and any other value is the sample duration. Not exposed over
    /// HTTP — it exists for tests.
    pub cpu_profile_duration: Option<std::time::Duration>,
}

// ---------------------------------------------------------------------------------------------
// The YAML files (goccy/go-yaml, through `crate::goyaml`)
// ---------------------------------------------------------------------------------------------

impl SupportPacketStats {
    /// `stats.yaml` — `yaml.Marshal(&stats)` (app/support_packet.go:221). No field is `omitempty`.
    pub fn to_yaml(&self) -> String {
        crate::goyaml::marshal(
            &MapBuilder::new()
                .field("registered_users", Node::Int(self.registered_users))
                .field("active_users", Node::Int(self.active_users))
                .field("daily_active_users", Node::Int(self.daily_active_users))
                .field("monthly_active_users", Node::Int(self.monthly_active_users))
                .field("deactivated_users", Node::Int(self.deactivated_users))
                .field("guests", Node::Int(self.guests))
                .field(
                    "single_channel_guests",
                    Node::Int(self.single_channel_guests),
                )
                .field("bot_accounts", Node::Int(self.bot_accounts))
                .field("posts", Node::Int(self.posts))
                .field("channels", Node::Int(self.channels))
                .field("teams", Node::Int(self.teams))
                .field("slash_commands", Node::Int(self.slash_commands))
                .field("incoming_webhooks", Node::Int(self.incoming_webhooks))
                .field("outgoing_webhooks", Node::Int(self.outgoing_webhooks))
                .build(),
        )
    }
}

/// Port of `(*Job).MarshalYAML` (job.go:118): the three timestamps through
/// `timeutils.FormatMillis` — in the **server's** zone, and quoted by the encoder because each
/// parses as an `RFC3339Nano` time — and `data` as a sorted map, `{}` when nil.
pub fn job_yaml_node<Tz: chrono::TimeZone>(job: &Job, tz: &Tz) -> Node {
    let empty = StringMap::new();
    MapBuilder::new()
        .field("id", Node::str(&job.id))
        .field("type", Node::str(&job.job_type))
        .field("priority", Node::Int(job.priority))
        .field(
            "create_at",
            Node::Str(crate::timeutils::format_millis_in(job.create_at, tz)),
        )
        .field(
            "start_at",
            Node::Str(crate::timeutils::format_millis_in(job.start_at, tz)),
        )
        .field(
            "last_activity_at",
            Node::Str(crate::timeutils::format_millis_in(job.last_activity_at, tz)),
        )
        .field("status", Node::str(&job.status))
        .field("progress", Node::Int(job.progress))
        .field(
            "data",
            Node::string_map(job.data.as_ref().unwrap_or(&empty)),
        )
        .build()
}

impl SupportPacketJobList {
    /// `jobs.yaml` (app/support_packet.go:267), timestamps in the server's local zone.
    pub fn to_yaml(&self) -> String {
        self.to_yaml_in(&chrono::Local)
    }

    /// [`SupportPacketJobList::to_yaml`] in an explicit zone, for the corpus test.
    pub fn to_yaml_in<Tz: chrono::TimeZone>(&self, tz: &Tz) -> String {
        let list = |jobs: &[Job]| Node::Seq(jobs.iter().map(|j| job_yaml_node(j, tz)).collect());
        crate::goyaml::marshal(
            &MapBuilder::new()
                .field("ldap_sync_jobs", list(&self.ldap_sync_jobs))
                .field("data_retention_jobs", list(&self.data_retention_jobs))
                .field("message_export_jobs", list(&self.message_export_jobs))
                .field(
                    "elastic_post_indexing_jobs",
                    list(&self.elastic_post_indexing_jobs),
                )
                .field(
                    "elastic_post_aggregation_jobs",
                    list(&self.elastic_post_aggregation_jobs),
                )
                .field("migration_jobs", list(&self.migration_jobs))
                .build(),
        )
    }
}

/// Port of `(*Role).MarshalYAML` (role.go:471). `scheme_id` is a pointer with no `omitempty`:
/// `null` when nil, and `""` — quoted — when it points at an empty string.
pub fn role_yaml_node<Tz: chrono::TimeZone>(role: &Role, tz: &Tz) -> Node {
    MapBuilder::new()
        .field("id", Node::str(&role.id))
        .field("name", Node::str(&role.name))
        .field("display_name", Node::str(&role.display_name))
        .field("description", Node::str(&role.description))
        .field(
            "create_at",
            Node::Str(crate::timeutils::format_millis_in(role.create_at, tz)),
        )
        .field(
            "update_at",
            Node::Str(crate::timeutils::format_millis_in(role.update_at, tz)),
        )
        .field(
            "delete_at",
            Node::Str(crate::timeutils::format_millis_in(role.delete_at, tz)),
        )
        .field(
            "permissions",
            Node::strings(role.permissions.as_deref().unwrap_or_default()),
        )
        .field("scheme_managed", Node::Bool(role.scheme_managed))
        .field("built_in", Node::Bool(role.built_in))
        .field(
            "scheme_id",
            role.scheme_id.as_deref().map_or(Node::Null, Node::str),
        )
        .build()
}

/// Port of `(*Scheme).MarshalYAML` (scheme.go:73).
pub fn scheme_yaml_node<Tz: chrono::TimeZone>(scheme: &Scheme, tz: &Tz) -> Node {
    MapBuilder::new()
        .field("id", Node::str(&scheme.id))
        .field("name", Node::str(&scheme.name))
        .field("display_name", Node::str(&scheme.display_name))
        .field("description", Node::str(&scheme.description))
        .field(
            "create_at",
            Node::Str(crate::timeutils::format_millis_in(scheme.create_at, tz)),
        )
        .field(
            "update_at",
            Node::Str(crate::timeutils::format_millis_in(scheme.update_at, tz)),
        )
        .field(
            "delete_at",
            Node::Str(crate::timeutils::format_millis_in(scheme.delete_at, tz)),
        )
        .field("scope", Node::str(&scheme.scope))
        .field(
            "default_team_admin_role",
            Node::str(&scheme.default_team_admin_role),
        )
        .field(
            "default_team_user_role",
            Node::str(&scheme.default_team_user_role),
        )
        .field(
            "default_channel_admin_role",
            Node::str(&scheme.default_channel_admin_role),
        )
        .field(
            "default_channel_user_role",
            Node::str(&scheme.default_channel_user_role),
        )
        .field(
            "default_team_guest_role",
            Node::str(&scheme.default_team_guest_role),
        )
        .field(
            "default_channel_guest_role",
            Node::str(&scheme.default_channel_guest_role),
        )
        .field(
            "default_playbook_admin_role",
            Node::str(&scheme.default_playbook_admin_role),
        )
        .field(
            "default_playbook_member_role",
            Node::str(&scheme.default_playbook_member_role),
        )
        .field(
            "default_run_admin_role",
            Node::str(&scheme.default_run_admin_role),
        )
        .field(
            "default_run_member_role",
            Node::str(&scheme.default_run_member_role),
        )
        .build()
}

impl SupportPacketPermissionInfo {
    /// `permissions.yaml` (app/support_packet.go:318) — roles first, as the struct declares them.
    pub fn to_yaml(&self) -> String {
        self.to_yaml_in(&chrono::Local)
    }

    /// [`SupportPacketPermissionInfo::to_yaml`] in an explicit zone, for the corpus test.
    pub fn to_yaml_in<Tz: chrono::TimeZone>(&self, tz: &Tz) -> String {
        crate::goyaml::marshal(
            &MapBuilder::new()
                .field(
                    "roles",
                    Node::Seq(self.roles.iter().map(|r| role_yaml_node(r, tz)).collect()),
                )
                .field(
                    "schemes",
                    Node::Seq(
                        self.schemes
                            .iter()
                            .map(|s| scheme_yaml_node(s, tz))
                            .collect(),
                    ),
                )
                .build(),
        )
    }
}

impl SupportPacketDatabaseSchema {
    /// `database_schema.yaml` (app/support_packet.go:385). `options`, `indexes`, `collation`
    /// and `max_length` are `omitempty`; `columns` and `is_nullable` are not.
    pub fn to_yaml(&self) -> String {
        let tables = self
            .tables
            .iter()
            .map(|t| {
                MapBuilder::new()
                    .field("name", Node::str(&t.name))
                    .field_unless(t.collation.is_empty(), "collation", Node::str(&t.collation))
                    .field_unless(
                        t.options.is_empty(),
                        "options",
                        Node::string_map(&t.options),
                    )
                    .field(
                        "columns",
                        Node::Seq(
                            t.columns
                                .iter()
                                .map(|c| {
                                    MapBuilder::new()
                                        .field("name", Node::str(&c.name))
                                        .field("data_type", Node::str(&c.data_type))
                                        .field_unless(
                                            c.max_length == 0,
                                            "max_length",
                                            Node::Int(c.max_length),
                                        )
                                        .field("is_nullable", Node::Bool(c.is_nullable))
                                        .build()
                                })
                                .collect(),
                        ),
                    )
                    .field_unless(
                        t.indexes.is_empty(),
                        "indexes",
                        Node::Seq(
                            t.indexes
                                .iter()
                                .map(|i| {
                                    MapBuilder::new()
                                        .field("name", Node::str(&i.name))
                                        .field("definition", Node::str(&i.definition))
                                        .build()
                                })
                                .collect(),
                        ),
                    )
                    .build()
            })
            .collect();
        crate::goyaml::marshal(
            &MapBuilder::new()
                .field_unless(
                    self.database_collation.is_empty(),
                    "database_collation",
                    Node::str(&self.database_collation),
                )
                .field_unless(
                    self.database_encoding.is_empty(),
                    "database_encoding",
                    Node::str(&self.database_encoding),
                )
                .field("tables", Node::Seq(tables))
                .build(),
        )
    }
}

/// goccy's `time.Time` text: `Format(time.RFC3339Nano)`, never quoted. A Go zero time is
/// `0001-01-01T00:00:00Z`.
fn time_node(t: Option<&chrono::DateTime<chrono::Utc>>) -> Node {
    let text = t
        .map(|t| t.fixed_offset())
        .and_then(|t| crate::utils::go_time::format(&t))
        .unwrap_or_else(|| "0001-01-01T00:00:00Z".to_owned());
    Node::Verbatim(text)
}

fn status_and_error(status: &str, error: &str) -> MapBuilder {
    MapBuilder::new()
        .field_unless(status.is_empty(), "status", Node::str(status))
        .field_unless(error.is_empty(), "error", Node::str(error))
}

/// `omitempty` on a struct: goccy drops it when every field would be dropped
/// (`isOmittedByOmitEmptyTag`, encode.go:790).
fn struct_unless_empty(parent: MapBuilder, key: &str, child: MapBuilder) -> MapBuilder {
    if child.is_empty() {
        parent
    } else {
        parent.field(key, child.build())
    }
}

impl SupportPacketDiagnostics {
    /// The `diagnostics.yaml` document **without** its comments, which are the platform's
    /// (`diagnosticsYAMLComments`) and attached by `mm_app::support_packet`.
    ///
    /// The `omitempty` rules are goccy's, which are stricter than `encoding/json`'s: a non-pointer
    /// struct whose fields would all be dropped is dropped itself (`notifications`,
    /// `oauth_providers` and each provider), and a zero `time.Time` is dropped through
    /// `IsZero` (`host_started_at`). `ldap`, `saml` and `elastic` carry no `omitempty`, so an
    /// all-empty one is written as `{}`.
    pub fn to_yaml_node(&self) -> Node {
        let l = &self.license;
        let license = MapBuilder::new()
            .field("company", Node::str(&l.company))
            .field("users", Node::Int(l.users))
            .field("sku_short_name", Node::str(&l.sku_short_name))
            .field_unless(!l.is_trial, "is_trial", Node::Bool(true))
            .field_unless(!l.is_gov_sku, "is_gov_sku", Node::Bool(true))
            .field_unless(!l.is_non_production, "is_non_production", Node::Bool(true));
        let s = &self.server;
        let server = MapBuilder::new()
            .field("os", Node::str(&s.os))
            .field("architecture", Node::str(&s.architecture))
            .field("hostname", Node::str(&s.hostname))
            .field("installation_type", Node::str(&s.installation_type))
            .field("cpu_cores", Node::Int(s.cpu_cores))
            .field("total_memory_mb", Node::Uint(s.total_memory_mb))
            .field_unless(
                s.container_cpu_limit == 0.0,
                "container_cpu_limit",
                Node::Float(s.container_cpu_limit),
            )
            .field_unless(
                s.container_memory_limit_mb == 0,
                "container_memory_limit_mb",
                Node::Uint(s.container_memory_limit_mb),
            )
            .field("process_id", Node::Int(s.process_id))
            .field("started_at", time_node(s.started_at.as_ref()))
            .field_unless(
                s.host_started_at.is_none(),
                "host_started_at",
                time_node(s.host_started_at.as_ref()),
            )
            .field("open_file_descriptors", Node::Int(s.open_file_descriptors))
            .field("max_file_descriptors", Node::Int(s.max_file_descriptors))
            .field("version", Node::str(&s.version))
            .field("build_hash", Node::str(&s.build_hash))
            .field("go_version", Node::str(&s.go_version));
        let d = &self.database;
        let opt_f = |v: Option<f64>| v.map(Node::Float);
        let opt_i = |v: Option<i64>| v.map(Node::Int);
        let mut database = MapBuilder::new()
            .field("type", Node::str(&d.type_))
            .field("version", Node::str(&d.version))
            .field("schema_version", Node::str(&d.schema_version))
            .field("master_connections", Node::Int(d.master_connections))
            .field("replica_connections", Node::Int(d.replica_connections))
            .field("search_connections", Node::Int(d.search_connections))
            .field(
                "master_connections_in_use",
                Node::Int(d.master_connections_in_use),
            )
            .field(
                "master_connections_idle",
                Node::Int(d.master_connections_idle),
            )
            .field(
                "master_pool_wait_count",
                Node::Int(d.master_pool_wait_count),
            )
            .field(
                "master_pool_wait_duration_ms",
                Node::Int(d.master_pool_wait_duration_ms),
            )
            .field(
                "master_connections_closed_max_idle",
                Node::Int(d.master_connections_closed_max_idle),
            )
            .field(
                "master_connections_closed_max_lifetime",
                Node::Int(d.master_connections_closed_max_lifetime),
            )
            .field(
                "replica_connections_in_use",
                Node::Int(d.replica_connections_in_use),
            )
            .field(
                "replica_connections_idle",
                Node::Int(d.replica_connections_idle),
            )
            .field(
                "replica_pool_wait_count",
                Node::Int(d.replica_pool_wait_count),
            )
            .field(
                "replica_pool_wait_duration_ms",
                Node::Int(d.replica_pool_wait_duration_ms),
            )
            .field(
                "replica_connections_closed_max_idle",
                Node::Int(d.replica_connections_closed_max_idle),
            )
            .field(
                "replica_connections_closed_max_lifetime",
                Node::Int(d.replica_connections_closed_max_lifetime),
            );
        // The pointers: `omitempty` drops nil only — a pointer to zero is written.
        for (key, value) in [
            ("cache_hit_ratio", opt_f(d.cache_hit_ratio)),
            ("deadlocks", opt_i(d.deadlocks)),
            ("temp_files", opt_i(d.temp_files)),
            ("temp_bytes_mb", opt_f(d.temp_bytes_mb)),
            ("rollbacks", opt_i(d.rollbacks)),
            (
                "idle_in_transaction_count",
                opt_i(d.idle_in_transaction_count),
            ),
            (
                "longest_query_duration_seconds",
                opt_f(d.longest_query_duration_seconds),
            ),
            ("waiting_for_lock_count", opt_i(d.waiting_for_lock_count)),
            ("posts_dead_tuples", opt_i(d.posts_dead_tuples)),
            (
                "posts_last_autovacuum",
                d.posts_last_autovacuum.as_ref().map(|t| time_node(Some(t))),
            ),
        ] {
            if let Some(value) = value {
                database = database.field(key, value);
            }
        }
        let f = &self.file_store;
        let file_store = MapBuilder::new()
            .field("file_status", Node::str(&f.status))
            .field_unless(f.error.is_empty(), "error", Node::str(&f.error))
            .field("file_driver", Node::str(&f.driver))
            .field_unless(
                f.filesystem_type.is_empty(),
                "filesystem_type",
                Node::str(&f.filesystem_type),
            )
            .field_unless(f.total_mb == 0, "total_mb", Node::Uint(f.total_mb))
            .field_unless(
                f.available_mb == 0,
                "available_mb",
                Node::Uint(f.available_mb),
            );
        let n = &self.notifications;
        let notifications = struct_unless_empty(
            struct_unless_empty(
                MapBuilder::new(),
                "email",
                status_and_error(&n.email.status, &n.email.error),
            ),
            "push",
            status_and_error(&n.push.status, &n.push.error),
        );
        let ldap = &self.ldap;
        let ldap = status_and_error(&ldap.status, &ldap.error)
            .field_unless(
                ldap.server_name.is_empty(),
                "server_name",
                Node::str(&ldap.server_name),
            )
            .field_unless(
                ldap.server_version.is_empty(),
                "server_version",
                Node::str(&ldap.server_version),
            );
        let saml = &self.saml;
        let saml = MapBuilder::new()
            .field_unless(
                saml.provider_type.is_empty(),
                "provider_type",
                Node::str(&saml.provider_type),
            )
            .field_unless(saml.status.is_empty(), "status", Node::str(&saml.status))
            .field_unless(saml.error.is_empty(), "error", Node::str(&saml.error));
        let es = &self.elastic_search;
        let elastic = MapBuilder::new()
            .field_unless(es.status.is_empty(), "status", Node::str(&es.status))
            .field_unless(es.backend.is_empty(), "backend", Node::str(&es.backend))
            .field_unless(
                es.server_version.is_empty(),
                "server_version",
                Node::str(&es.server_version),
            )
            .field_unless(
                es.server_plugins.is_empty(),
                "server_plugins",
                Node::strings(&es.server_plugins),
            )
            .field_unless(es.error.is_empty(), "error", Node::str(&es.error));
        let o = &self.oauth_providers;
        let mut oauth = MapBuilder::new();
        for (key, provider) in [
            ("gitlab", &o.gitlab),
            ("google", &o.google),
            ("office365", &o.office365),
            ("openid", &o.openid),
        ] {
            oauth = struct_unless_empty(
                oauth,
                key,
                status_and_error(&provider.status, &provider.error),
            );
        }

        let doc = MapBuilder::new()
            .field("version", Node::Int(self.version))
            .field("license", license.build())
            .field("server", server.build())
            .field(
                "config",
                MapBuilder::new()
                    .field("store_type", Node::str(&self.config.source))
                    .build(),
            )
            .field("database", database.build())
            .field("file_store", file_store.build())
            .field(
                "websocket",
                MapBuilder::new()
                    .field("connections", Node::Int(self.websocket.connections))
                    .build(),
            )
            .field(
                "cluster",
                MapBuilder::new()
                    .field("id", Node::str(&self.cluster.id))
                    .field("number_of_nodes", Node::Int(self.cluster.number_of_nodes))
                    .build(),
            );
        let doc = struct_unless_empty(doc, "notifications", notifications)
            .field("ldap", ldap.build())
            .field("saml", saml.build())
            .field("elastic", elastic.build());
        struct_unless_empty(doc, "oauth_providers", oauth).build()
    }
}

/// The YAML and JSON files against goccy's and `encoding/json`'s own bytes
/// (`fixtures/behaviour_goyaml.json`, reference/dump/behaviour_support_packet.go).
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn corpus() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_goyaml.json"))
            .expect("behaviour_goyaml.json is generated by reference/dump")
    }

    fn zone(corpus: &Value) -> chrono::FixedOffset {
        let secs = corpus["zone_offset_secs"].as_i64().expect("offset") as i32;
        chrono::FixedOffset::east_opt(secs).expect("a valid offset")
    }

    #[test]
    fn stats_match_goccy() {
        let corpus = corpus();
        let stats = SupportPacketStats {
            registered_users: 101,
            active_users: 102,
            daily_active_users: 103,
            monthly_active_users: 104,
            deactivated_users: 105,
            guests: 106,
            single_channel_guests: 107,
            bot_accounts: 108,
            posts: 109,
            channels: 110,
            teams: 111,
            slash_commands: 112,
            incoming_webhooks: 113,
            outgoing_webhooks: 114,
        };
        assert_eq!(
            Some(stats.to_yaml().as_str()),
            corpus["stats"]["populated"].as_str()
        );
        assert_eq!(
            Some(SupportPacketStats::default().to_yaml().as_str()),
            corpus["stats"]["zero"].as_str()
        );
    }

    #[test]
    fn jobs_match_goccy() {
        let corpus = corpus();
        let tz = zone(&corpus);
        let jobs: Vec<Job> =
            serde_json::from_value(corpus["jobs"]["input_jobs"].clone()).expect("jobs");
        let list = SupportPacketJobList {
            ldap_sync_jobs: jobs[..1].to_vec(),
            data_retention_jobs: vec![],
            message_export_jobs: jobs[2..].to_vec(),
            elastic_post_indexing_jobs: vec![],
            elastic_post_aggregation_jobs: jobs.clone(),
            migration_jobs: jobs[1..2].to_vec(),
        };
        assert_eq!(
            Some(list.to_yaml_in(&tz).as_str()),
            corpus["jobs"]["yaml"].as_str()
        );
        assert_eq!(
            Some(SupportPacketJobList::default().to_yaml_in(&tz).as_str()),
            corpus["jobs"]["empty"].as_str()
        );
    }

    #[test]
    fn permissions_match_goccy() {
        let corpus = corpus();
        let tz = zone(&corpus);
        let mut roles: Vec<Role> =
            serde_json::from_value(corpus["permissions"]["input_roles"].clone()).expect("roles");
        let mut schemes: Vec<Scheme> =
            serde_json::from_value(corpus["permissions"]["input_schemes"].clone())
                .expect("schemes");
        roles.iter_mut().for_each(Role::sanitize);
        schemes.iter_mut().for_each(Scheme::sanitize);
        let info = SupportPacketPermissionInfo {
            roles: roles.clone(),
            schemes,
        };
        assert_eq!(
            Some(info.to_yaml_in(&tz).as_str()),
            corpus["permissions"]["yaml"].as_str()
        );
        let only = SupportPacketPermissionInfo {
            roles: roles[..1].to_vec(),
            schemes: vec![],
        };
        assert_eq!(
            Some(only.to_yaml_in(&tz).as_str()),
            corpus["permissions"]["no_schemes"].as_str()
        );
    }

    #[test]
    fn schema_matches_goccy() {
        let corpus = corpus();
        let schema = SupportPacketDatabaseSchema {
            database_collation: "en_US.utf8".into(),
            database_encoding: "UTF8".into(),
            tables: vec![
                DatabaseTable {
                    name: "posts".into(),
                    collation: "default".into(),
                    options: [("fillfactor", "90"), ("autovacuum_enabled", "true")]
                        .iter()
                        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                        .collect(),
                    columns: vec![
                        DatabaseColumn {
                            name: "id".into(),
                            data_type: "character varying".into(),
                            max_length: 26,
                            is_nullable: false,
                        },
                        DatabaseColumn {
                            name: "props".into(),
                            data_type: "jsonb".into(),
                            max_length: 0,
                            is_nullable: true,
                        },
                    ],
                    indexes: vec![DatabaseIndex {
                        name: "posts_pkey".into(),
                        definition:
                            "CREATE UNIQUE INDEX posts_pkey ON public.posts USING btree (id)".into(),
                    }],
                },
                DatabaseTable {
                    name: "empty".into(),
                    ..DatabaseTable::default()
                },
            ],
        };
        assert_eq!(
            Some(schema.to_yaml().as_str()),
            corpus["schema"]["yaml"].as_str()
        );
        assert_eq!(
            Some(SupportPacketDatabaseSchema::default().to_yaml().as_str()),
            corpus["schema"]["empty"].as_str()
        );
    }

    #[test]
    fn sanitize_data_source_matches_go() {
        for case in corpus()["sanitize_data_source"].as_array().expect("cases") {
            let driver = case["driver"].as_str().expect("driver");
            let input = case["input"].as_str().expect("input");
            let got = crate::config::sanitize_data_source(driver, input);
            match case["error"].as_str() {
                Some(error) => assert_eq!(
                    got.map_err(|e| e.to_string()),
                    Err(error.to_owned()),
                    "{driver} {input:?}"
                ),
                None => assert_eq!(
                    got.ok().as_deref(),
                    case["output"].as_str(),
                    "{driver} {input:?}"
                ),
            }
        }
    }

    #[test]
    fn sanitize_file_name_matches_go() {
        for case in corpus()["sanitize_file_name"].as_array().expect("cases") {
            let input = case["input"].as_str().expect("input");
            assert_eq!(
                Some(crate::utils::sanitize_file_name(input).as_str()),
                case["output"].as_str(),
                "{input:?}"
            );
        }
    }

    #[test]
    fn plugin_settings_sanitize_matches_go() {
        let corpus = corpus();
        let case = &corpus["plugin_settings_sanitize"];
        let manifests: Vec<Manifest> =
            serde_json::from_value(case["manifests"].clone()).expect("manifests");
        let input: std::collections::BTreeMap<String, crate::utils::StringInterface> =
            serde_json::from_value(case["input"].clone()).expect("input");
        let mut with = crate::config::PluginSettings {
            plugins: Some(input.clone()),
            ..Default::default()
        };
        with.sanitize(Some(&manifests));
        assert_eq!(
            serde_json::to_value(&with.plugins).expect("json"),
            case["with_manifests"]
        );
        let mut without = crate::config::PluginSettings {
            plugins: Some(input),
            ..Default::default()
        };
        without.sanitize(None);
        assert_eq!(
            serde_json::to_value(&without.plugins).expect("json"),
            case["with_nil"]
        );
    }

    /// The flags are written once, and last — where Go's dominating outer field puts them.
    #[test]
    fn feature_flags_are_the_last_key_and_appear_once() {
        let config = Config {
            feature_flags: Some(FeatureFlags::default()),
            ..Config::default()
        };
        let text = crate::utils::go_json_marshal_indent(&SupportPacketConfig::new(config))
            .expect("marshals");
        assert_eq!(text.matches("\"FeatureFlags\"").count(), 1);
        // The last line indented exactly one level is the last top-level key.
        let last_top_level = text
            .lines()
            .rfind(|l| l.starts_with("    \""))
            .expect("top-level keys");
        assert!(
            last_top_level.starts_with("    \"FeatureFlags\""),
            "{last_top_level}"
        );
    }

    #[test]
    fn plugin_list_matches_marshal_indent() {
        let corpus = corpus();
        let empty = SupportPacketPluginList::default();
        assert_eq!(
            crate::utils::go_json_marshal_indent(&empty).ok().as_deref(),
            corpus["plugin_list"]["empty"].as_str()
        );
        let one = SupportPacketPluginList {
            enabled: Some(vec![Manifest {
                id: "a<b>&c".into(),
                name: "N".into(),
                version: "1.0.0".into(),
                ..Manifest::default()
            }]),
            disabled: None,
        };
        assert_eq!(
            crate::utils::go_json_marshal_indent(&one).ok().as_deref(),
            corpus["plugin_list"]["one"].as_str()
        );
    }
}

#[cfg(test)]
mod wire_parity {
    use super::*;

    /// Round-trips the Go-generated fixture: decode into the port's type, re-encode, and compare
    /// the value graphs. The fixture is produced by `reference/dump`, whose reflective filler
    /// gives **every** field a distinctive non-zero value — so a dropped key, a renamed tag or a
    /// mis-typed field cannot pass. This is the parity oracle, not a smoke test.
    macro_rules! assert_fixture_round_trips {
        ($ty:ty, $fixture:literal) => {{
            let raw = include_str!(concat!("../../../fixtures/", $fixture, ".json"));
            let decoded: $ty =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("decoding {}: {e}", $fixture));
            let expected: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(
                serde_json::to_value(&decoded).unwrap(),
                expected,
                "re-encoding {} does not match Go",
                $fixture
            );
        }};
    }

    #[test]
    fn support_packet_config_round_trips_the_fixture() {
        assert_fixture_round_trips!(SupportPacketConfig, "support_packet_config");
    }
    #[test]
    fn support_packet_plugin_list_round_trips_the_fixture() {
        assert_fixture_round_trips!(SupportPacketPluginList, "support_packet_plugin_list");
    }
}
