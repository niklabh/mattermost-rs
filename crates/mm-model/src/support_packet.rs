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
/// document with one extra top-level key, `FeatureFlags`. `Config` itself does not carry the flags
/// on the wire, which is the whole reason this type exists.
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

/// Port of `model.SupportPacketPluginList` (support_packet.go:198). **JSON**, keys `enabled` and
/// `disabled`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SupportPacketPluginList {
    #[serde(rename = "enabled")]
    pub enabled: Vec<Manifest>,

    #[serde(rename = "disabled")]
    pub disabled: Vec<Manifest>,
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
