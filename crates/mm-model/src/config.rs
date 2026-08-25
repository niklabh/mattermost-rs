//! Port of `model/config.go` — the server configuration.
//!
//! # The wire keys are the Go **field names**
//!
//! `model.Config` and every settings struct under it carry no `json:` tag, so `encoding/json`
//! uses the exported field name verbatim: `ServiceSettings`, `SiteURL`, `EnableAPIv3`. Three
//! fields are the only exceptions, and they carry `json:",omitempty"` — an empty name, meaning
//! "keep the field name, but drop me when nil": `ServiceSettings.EnableExperimentalGossipEncryption`,
//! `ElasticsearchSettings.BulkIndexingTimeWindowSeconds` and `Config.FeatureFlags`. **Every other
//! nil pointer is written as `null`**, which is what makes a config document round-trip: the
//! server distinguishes "not configured" from "configured to the zero value", and `SetDefaults`
//! resolves the former.
//!
//! # Scope of this port
//!
//! This is the **wire shape only** — 53 structs and roughly 1,300 fields, generated from the Go
//! source rather than transcribed, because a hand-copy of that many names is where a silent key
//! typo comes from.
//!
//! `SetDefaults`, `IsValid` and the `Sanitize`/`Clone` family are **not ported**. They are
//! several thousand lines of per-field logic with no test oracle in this crate yet, and
//! `MIGRATION_STRATEGY.md`'s guidance for `config.go` is explicit: *"translate lazily, section by
//! section"*. A route that needs a section brings that section's defaults and validation with it.
//! Until then a `Config` decoded here is exactly what was on the wire, with no defaults applied —
//! which is the right behaviour for the Strangler Fig proxy, where the Go server owns the config.
//!
//! The `access:"…"` tags drive the System Console's permission model and have no JSON effect.

use serde::{Deserialize, Serialize};

use crate::ai_recap_settings::AIRecapSettings;
use crate::content_flagging_settings::ContentFlaggingSettings;
use crate::feature_flags::FeatureFlags;
use crate::utils::StringInterface;

/// Port of `model.ServiceSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServiceSettings {
    #[serde(rename = "SiteURL")]
    pub site_url: Option<String>,

    #[serde(rename = "WebsocketURL")]
    pub websocket_url: Option<String>,

    #[serde(rename = "LicenseFileLocation")]
    pub license_file_location: Option<String>,

    #[serde(rename = "ListenAddress")]
    pub listen_address: Option<String>,

    #[serde(rename = "ConnectionSecurity")]
    pub connection_security: Option<String>,

    #[serde(rename = "TLSCertFile")]
    pub tls_cert_file: Option<String>,

    #[serde(rename = "TLSKeyFile")]
    pub tls_key_file: Option<String>,

    #[serde(rename = "TLSMinVer")]
    pub tls_min_ver: Option<String>,

    #[serde(rename = "TLSStrictTransport")]
    pub tls_strict_transport: Option<bool>,

    #[serde(rename = "TLSStrictTransportMaxAge")]
    pub tls_strict_transport_max_age: Option<i64>,

    #[serde(rename = "TLSOverwriteCiphers")]
    pub tls_overwrite_ciphers: Option<Vec<String>>,

    #[serde(rename = "UseLetsEncrypt")]
    pub use_lets_encrypt: Option<bool>,

    #[serde(rename = "LetsEncryptCertificateCacheFile")]
    pub lets_encrypt_certificate_cache_file: Option<String>,

    #[serde(rename = "Forward80To443")]
    pub forward80_to443: Option<bool>,

    #[serde(rename = "TrustedProxyIPHeader")]
    pub trusted_proxy_ip_header: Option<Vec<String>>,

    #[serde(rename = "ReadTimeout")]
    pub read_timeout: Option<i64>,

    #[serde(rename = "WriteTimeout")]
    pub write_timeout: Option<i64>,

    #[serde(rename = "IdleTimeout")]
    pub idle_timeout: Option<i64>,

    #[serde(rename = "MaximumLoginAttempts")]
    pub maximum_login_attempts: Option<i64>,

    #[serde(rename = "GoroutineHealthThreshold")]
    pub goroutine_health_threshold: Option<i64>,

    #[serde(rename = "EnableOAuthServiceProvider")]
    pub enable_o_auth_service_provider: Option<bool>,

    #[serde(rename = "EnableDynamicClientRegistration")]
    pub enable_dynamic_client_registration: Option<bool>,

    #[serde(rename = "DCRRedirectURIAllowlist")]
    pub dcr_redirect_uri_allowlist: Option<Vec<String>>,

    #[serde(rename = "EnableIncomingWebhooks")]
    pub enable_incoming_webhooks: Option<bool>,

    #[serde(rename = "EnableOutgoingWebhooks")]
    pub enable_outgoing_webhooks: Option<bool>,

    #[serde(rename = "EnableOutgoingOAuthConnections")]
    pub enable_outgoing_o_auth_connections: Option<bool>,

    #[serde(rename = "EnableCommands")]
    pub enable_commands: Option<bool>,

    #[serde(rename = "OutgoingIntegrationRequestsTimeout")]
    pub outgoing_integration_requests_timeout: Option<i64>,

    #[serde(rename = "EnablePostUsernameOverride")]
    pub enable_post_username_override: Option<bool>,

    #[serde(rename = "EnablePostIconOverride")]
    pub enable_post_icon_override: Option<bool>,

    #[serde(rename = "GoogleDeveloperKey")]
    pub google_developer_key: Option<String>,

    #[serde(rename = "EnableLinkPreviews")]
    pub enable_link_previews: Option<bool>,

    #[serde(rename = "EnablePermalinkPreviews")]
    pub enable_permalink_previews: Option<bool>,

    #[serde(rename = "RestrictLinkPreviews")]
    pub restrict_link_previews: Option<String>,

    #[serde(rename = "EnableTesting")]
    pub enable_testing: Option<bool>,

    #[serde(rename = "EnableDeveloper")]
    pub enable_developer: Option<bool>,

    #[serde(rename = "DeveloperFlags")]
    pub developer_flags: Option<String>,

    #[serde(rename = "EnableClientPerformanceDebugging")]
    pub enable_client_performance_debugging: Option<bool>,

    #[serde(rename = "EnableSecurityFixAlert")]
    pub enable_security_fix_alert: Option<bool>,

    #[serde(rename = "EnableInsecureOutgoingConnections")]
    pub enable_insecure_outgoing_connections: Option<bool>,

    #[serde(rename = "AllowedUntrustedInternalConnections")]
    pub allowed_untrusted_internal_connections: Option<String>,

    #[serde(rename = "EnableMultifactorAuthentication")]
    pub enable_multifactor_authentication: Option<bool>,

    #[serde(rename = "EnforceMultifactorAuthentication")]
    pub enforce_multifactor_authentication: Option<bool>,

    #[serde(rename = "EnableUserAccessTokens")]
    pub enable_user_access_tokens: Option<bool>,

    #[serde(rename = "MaximumPersonalAccessTokenLifetimeDays")]
    pub maximum_personal_access_token_lifetime_days: Option<i64>,

    #[serde(rename = "AllowCorsFrom")]
    pub allow_cors_from: Option<String>,

    #[serde(rename = "CorsExposedHeaders")]
    pub cors_exposed_headers: Option<String>,

    #[serde(rename = "CorsAllowCredentials")]
    pub cors_allow_credentials: Option<bool>,

    #[serde(rename = "CorsDebug")]
    pub cors_debug: Option<bool>,

    #[serde(rename = "AllowCookiesForSubdomains")]
    pub allow_cookies_for_subdomains: Option<bool>,

    #[serde(rename = "ExtendSessionLengthWithActivity")]
    pub extend_session_length_with_activity: Option<bool>,

    #[serde(rename = "TerminateSessionsOnPasswordChange")]
    pub terminate_sessions_on_password_change: Option<bool>,

    #[serde(rename = "SessionLengthWebInDays")]
    pub session_length_web_in_days: Option<i64>,

    #[serde(rename = "SessionLengthWebInHours")]
    pub session_length_web_in_hours: Option<i64>,

    #[serde(rename = "SessionLengthMobileInDays")]
    pub session_length_mobile_in_days: Option<i64>,

    #[serde(rename = "SessionLengthMobileInHours")]
    pub session_length_mobile_in_hours: Option<i64>,

    #[serde(rename = "SessionLengthSSOInDays")]
    pub session_length_sso_in_days: Option<i64>,

    #[serde(rename = "SessionLengthSSOInHours")]
    pub session_length_sso_in_hours: Option<i64>,

    #[serde(rename = "SessionCacheInMinutes")]
    pub session_cache_in_minutes: Option<i64>,

    #[serde(rename = "SessionIdleTimeoutInMinutes")]
    pub session_idle_timeout_in_minutes: Option<i64>,

    #[serde(rename = "WebsocketSecurePort")]
    pub websocket_secure_port: Option<i64>,

    #[serde(rename = "WebsocketPort")]
    pub websocket_port: Option<i64>,

    #[serde(rename = "WebserverMode")]
    pub webserver_mode: Option<String>,

    #[serde(rename = "EnableGifPicker")]
    pub enable_gif_picker: Option<bool>,

    #[serde(rename = "GiphySdkKey")]
    pub giphy_sdk_key: Option<String>,

    #[serde(rename = "EnableCustomEmoji")]
    pub enable_custom_emoji: Option<bool>,

    #[serde(rename = "EnableEmojiPicker")]
    pub enable_emoji_picker: Option<bool>,

    #[serde(rename = "PostEditTimeLimit")]
    pub post_edit_time_limit: Option<i64>,

    #[serde(rename = "TimeBetweenUserTypingUpdatesMilliseconds")]
    pub time_between_user_typing_updates_milliseconds: Option<i64>,

    #[serde(rename = "EnableCrossTeamSearch")]
    pub enable_cross_team_search: Option<bool>,

    #[serde(rename = "EnablePostSearch")]
    pub enable_post_search: Option<bool>,

    #[serde(rename = "EnableFileSearch")]
    pub enable_file_search: Option<bool>,

    #[serde(rename = "MinimumHashtagLength")]
    pub minimum_hashtag_length: Option<i64>,

    #[serde(rename = "EnableUserTypingMessages")]
    pub enable_user_typing_messages: Option<bool>,

    #[serde(rename = "EnableChannelViewedMessages")]
    pub enable_channel_viewed_messages: Option<bool>,

    #[serde(rename = "EnableUserStatuses")]
    pub enable_user_statuses: Option<bool>,

    #[serde(rename = "ExperimentalEnableAuthenticationTransfer")]
    pub experimental_enable_authentication_transfer: Option<bool>,

    #[serde(rename = "ClusterLogTimeoutMilliseconds")]
    pub cluster_log_timeout_milliseconds: Option<i64>,

    #[serde(rename = "EnableTutorial")]
    pub enable_tutorial: Option<bool>,

    #[serde(rename = "EnableOnboardingFlow")]
    pub enable_onboarding_flow: Option<bool>,

    #[serde(rename = "ExperimentalEnableDefaultChannelLeaveJoinMessages")]
    pub experimental_enable_default_channel_leave_join_messages: Option<bool>,

    #[serde(rename = "ExperimentalGroupUnreadChannels")]
    pub experimental_group_unread_channels: Option<String>,

    #[serde(rename = "EnableAPITeamDeletion")]
    pub enable_api_team_deletion: Option<bool>,

    #[serde(rename = "EnableAPITriggerAdminNotifications")]
    pub enable_api_trigger_admin_notifications: Option<bool>,

    #[serde(rename = "EnableAPIUserDeletion")]
    pub enable_api_user_deletion: Option<bool>,

    #[serde(rename = "EnableAPIPostDeletion")]
    pub enable_api_post_deletion: Option<bool>,

    #[serde(rename = "EnableDesktopLandingPage")]
    pub enable_desktop_landing_page: Option<bool>,

    #[serde(rename = "MinimumDesktopAppVersion")]
    pub minimum_desktop_app_version: Option<String>,

    #[serde(rename = "ExperimentalEnableHardenedMode")]
    pub experimental_enable_hardened_mode: Option<bool>,

    #[serde(rename = "ExperimentalStrictCSRFEnforcement")]
    pub experimental_strict_csrf_enforcement: Option<bool>,

    #[serde(rename = "EnableEmailInvitations")]
    pub enable_email_invitations: Option<bool>,

    #[serde(rename = "DisableBotsWhenOwnerIsDeactivated")]
    pub disable_bots_when_owner_is_deactivated: Option<bool>,

    #[serde(rename = "EnableBotAccountCreation")]
    pub enable_bot_account_creation: Option<bool>,

    #[serde(rename = "EnableSVGs")]
    pub enable_sv_gs: Option<bool>,

    #[serde(rename = "EnableLatex")]
    pub enable_latex: Option<bool>,

    #[serde(rename = "EnableInlineLatex")]
    pub enable_inline_latex: Option<bool>,

    #[serde(rename = "PostPriority")]
    pub post_priority: Option<bool>,

    #[serde(rename = "AllowPersistentNotifications")]
    pub allow_persistent_notifications: Option<bool>,

    #[serde(rename = "AllowPersistentNotificationsForGuests")]
    pub allow_persistent_notifications_for_guests: Option<bool>,

    #[serde(rename = "PersistentNotificationIntervalMinutes")]
    pub persistent_notification_interval_minutes: Option<i64>,

    #[serde(rename = "PersistentNotificationMaxCount")]
    pub persistent_notification_max_count: Option<i64>,

    #[serde(rename = "PersistentNotificationMaxRecipients")]
    pub persistent_notification_max_recipients: Option<i64>,

    #[serde(rename = "EnableBurnOnRead")]
    pub enable_burn_on_read: Option<bool>,

    #[serde(rename = "BurnOnReadDurationSeconds")]
    pub burn_on_read_duration_seconds: Option<i64>,

    #[serde(rename = "BurnOnReadMaximumTimeToLiveSeconds")]
    pub burn_on_read_maximum_time_to_live_seconds: Option<i64>,

    #[serde(rename = "BurnOnReadSchedulerFrequencySeconds")]
    pub burn_on_read_scheduler_frequency_seconds: Option<i64>,

    #[serde(rename = "EnableAPIChannelDeletion")]
    pub enable_api_channel_deletion: Option<bool>,

    #[serde(rename = "EnableLocalMode")]
    pub enable_local_mode: Option<bool>,

    #[serde(rename = "LocalModeSocketLocation")]
    pub local_mode_socket_location: Option<String>,

    #[serde(rename = "EnableAWSMetering")]
    pub enable_aws_metering: Option<bool>,

    #[serde(rename = "AWSMeteringTimeoutSeconds")]
    pub aws_metering_timeout_seconds: Option<i64>,

    #[serde(rename = "SplitKey")]
    pub split_key: Option<String>,

    #[serde(rename = "FeatureFlagSyncIntervalSeconds")]
    pub feature_flag_sync_interval_seconds: Option<i64>,

    #[serde(rename = "DebugSplit")]
    pub debug_split: Option<bool>,

    #[serde(rename = "ThreadAutoFollow")]
    pub thread_auto_follow: Option<bool>,

    #[serde(rename = "CollapsedThreads")]
    pub collapsed_threads: Option<String>,

    #[serde(rename = "ManagedResourcePaths")]
    pub managed_resource_paths: Option<String>,

    #[serde(rename = "EnableCustomGroups")]
    pub enable_custom_groups: Option<bool>,

    #[serde(rename = "AllowSyncedDrafts")]
    pub allow_synced_drafts: Option<bool>,

    #[serde(rename = "UniqueEmojiReactionLimitPerPost")]
    pub unique_emoji_reaction_limit_per_post: Option<i64>,

    #[serde(rename = "RefreshPostStatsRunTime")]
    pub refresh_post_stats_run_time: Option<String>,

    #[serde(rename = "MaximumPayloadSizeBytes")]
    pub maximum_payload_size_bytes: Option<i64>,

    #[serde(rename = "MaximumURLLength")]
    pub maximum_url_length: Option<i64>,

    #[serde(rename = "ScheduledPosts")]
    pub scheduled_posts: Option<bool>,

    #[serde(rename = "EnableWebHubChannelIteration")]
    pub enable_web_hub_channel_iteration: Option<bool>,

    #[serde(rename = "FrameAncestors")]
    pub frame_ancestors: Option<String>,

    #[serde(rename = "DeleteAccountLink")]
    pub delete_account_link: Option<String>,
}

/// Port of `model.CacheSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheSettings {
    #[serde(rename = "CacheType")]
    pub cache_type: Option<String>,

    #[serde(rename = "RedisAddress")]
    pub redis_address: Option<String>,

    #[serde(rename = "RedisPassword")]
    pub redis_password: Option<String>,

    #[serde(rename = "RedisDB")]
    pub redis_db: Option<i64>,

    #[serde(rename = "RedisCachePrefix")]
    pub redis_cache_prefix: Option<String>,

    #[serde(rename = "DisableClientCache")]
    pub disable_client_cache: Option<bool>,
}

/// Port of `model.ClusterSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClusterSettings {
    #[serde(rename = "Enable")]
    pub enable: Option<bool>,

    #[serde(rename = "ClusterName")]
    pub cluster_name: Option<String>,

    #[serde(rename = "OverrideHostname")]
    pub override_hostname: Option<String>,

    #[serde(rename = "NetworkInterface")]
    pub network_interface: Option<String>,

    #[serde(rename = "BindAddress")]
    pub bind_address: Option<String>,

    #[serde(rename = "AdvertiseAddress")]
    pub advertise_address: Option<String>,

    #[serde(rename = "UseIPAddress")]
    pub use_ip_address: Option<bool>,

    #[serde(rename = "EnableGossipCompression")]
    pub enable_gossip_compression: Option<bool>,

    #[serde(
        rename = "EnableExperimentalGossipEncryption",
        skip_serializing_if = "Option::is_none"
    )]
    pub enable_experimental_gossip_encryption: Option<bool>,

    #[serde(rename = "EnableGossipEncryption")]
    pub enable_gossip_encryption: Option<bool>,

    #[serde(rename = "ReadOnlyConfig")]
    pub read_only_config: Option<bool>,

    #[serde(rename = "GossipPort")]
    pub gossip_port: Option<i64>,
}

/// Port of `model.MetricsSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricsSettings {
    #[serde(rename = "Enable")]
    pub enable: Option<bool>,

    #[serde(rename = "BlockProfileRate")]
    pub block_profile_rate: Option<i64>,

    #[serde(rename = "ListenAddress")]
    pub listen_address: Option<String>,

    #[serde(rename = "EnableClientMetrics")]
    pub enable_client_metrics: Option<bool>,

    #[serde(rename = "EnableNotificationMetrics")]
    pub enable_notification_metrics: Option<bool>,

    #[serde(rename = "ClientSideUserIds")]
    pub client_side_user_ids: Option<Vec<String>>,
}

/// Port of `model.ExperimentalSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExperimentalSettings {
    #[serde(rename = "ClientSideCertEnable")]
    pub client_side_cert_enable: Option<bool>,

    #[serde(rename = "LinkMetadataTimeoutMilliseconds")]
    pub link_metadata_timeout_milliseconds: Option<i64>,

    #[serde(rename = "RestrictSystemAdmin")]
    pub restrict_system_admin: Option<bool>,

    #[serde(rename = "EnableSharedChannels")]
    pub enable_shared_channels: Option<bool>,

    #[serde(rename = "EnableRemoteClusterService")]
    pub enable_remote_cluster_service: Option<bool>,

    #[serde(rename = "DisableAppBar")]
    pub disable_app_bar: Option<bool>,

    #[serde(rename = "DisableRefetchingOnBrowserFocus")]
    pub disable_refetching_on_browser_focus: Option<bool>,

    #[serde(rename = "DelayChannelAutocomplete")]
    pub delay_channel_autocomplete: Option<bool>,

    #[serde(rename = "DisableWakeUpReconnectHandler")]
    pub disable_wake_up_reconnect_handler: Option<bool>,

    #[serde(rename = "UsersStatusAndProfileFetchingPollIntervalMilliseconds")]
    pub users_status_and_profile_fetching_poll_interval_milliseconds: Option<i64>,

    #[serde(rename = "YoutubeReferrerPolicy")]
    pub youtube_referrer_policy: Option<bool>,

    #[serde(rename = "EnableWatermark")]
    pub enable_watermark: Option<bool>,
}

/// Port of `model.AnalyticsSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnalyticsSettings {
    #[serde(rename = "MaxUsersForStatistics")]
    pub max_users_for_statistics: Option<i64>,
}

/// Port of `model.SSOSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SSOSettings {
    #[serde(rename = "Enable")]
    pub enable: Option<bool>,

    #[serde(rename = "Secret")]
    pub secret: Option<String>,

    #[serde(rename = "Id")]
    pub id: Option<String>,

    #[serde(rename = "Scope")]
    pub scope: Option<String>,

    #[serde(rename = "AuthEndpoint")]
    pub auth_endpoint: Option<String>,

    #[serde(rename = "TokenEndpoint")]
    pub token_endpoint: Option<String>,

    #[serde(rename = "UserAPIEndpoint")]
    pub user_api_endpoint: Option<String>,

    #[serde(rename = "DiscoveryEndpoint")]
    pub discovery_endpoint: Option<String>,

    #[serde(rename = "ButtonText")]
    pub button_text: Option<String>,

    #[serde(rename = "ButtonColor")]
    pub button_color: Option<String>,

    #[serde(rename = "UsePreferredUsername")]
    pub use_preferred_username: Option<bool>,
}

/// Port of `model.Office365Settings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Office365Settings {
    #[serde(rename = "Enable")]
    pub enable: Option<bool>,

    #[serde(rename = "Secret")]
    pub secret: Option<String>,

    #[serde(rename = "Id")]
    pub id: Option<String>,

    #[serde(rename = "Scope")]
    pub scope: Option<String>,

    #[serde(rename = "AuthEndpoint")]
    pub auth_endpoint: Option<String>,

    #[serde(rename = "TokenEndpoint")]
    pub token_endpoint: Option<String>,

    #[serde(rename = "UserAPIEndpoint")]
    pub user_api_endpoint: Option<String>,

    #[serde(rename = "DiscoveryEndpoint")]
    pub discovery_endpoint: Option<String>,

    #[serde(rename = "DirectoryId")]
    pub directory_id: Option<String>,

    #[serde(rename = "UsePreferredUsername")]
    pub use_preferred_username: Option<bool>,
}

/// Port of `model.IntuneSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IntuneSettings {
    #[serde(rename = "Enable")]
    pub enable: Option<bool>,

    #[serde(rename = "TenantId")]
    pub tenant_id: Option<String>,

    #[serde(rename = "ClientId")]
    pub client_id: Option<String>,

    #[serde(rename = "AuthService")]
    pub auth_service: Option<String>,
}

/// Port of `model.ReplicaLagSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReplicaLagSettings {
    #[serde(rename = "DataSource")]
    pub data_source: Option<String>,

    #[serde(rename = "QueryAbsoluteLag")]
    pub query_absolute_lag: Option<String>,

    #[serde(rename = "QueryTimeLag")]
    pub query_time_lag: Option<String>,
}

/// Port of `model.SqlSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SqlSettings {
    #[serde(rename = "DriverName")]
    pub driver_name: Option<String>,

    #[serde(rename = "DataSource")]
    pub data_source: Option<String>,

    #[serde(rename = "DataSourceReplicas")]
    pub data_source_replicas: Option<Vec<String>>,

    #[serde(rename = "DataSourceSearchReplicas")]
    pub data_source_search_replicas: Option<Vec<String>>,

    #[serde(rename = "MaxIdleConns")]
    pub max_idle_conns: Option<i64>,

    #[serde(rename = "ConnMaxLifetimeMilliseconds")]
    pub conn_max_lifetime_milliseconds: Option<i64>,

    #[serde(rename = "ConnMaxIdleTimeMilliseconds")]
    pub conn_max_idle_time_milliseconds: Option<i64>,

    #[serde(rename = "MaxOpenConns")]
    pub max_open_conns: Option<i64>,

    #[serde(rename = "Trace")]
    pub trace: Option<bool>,

    #[serde(rename = "AtRestEncryptKey")]
    pub at_rest_encrypt_key: Option<String>,

    #[serde(rename = "QueryTimeout")]
    pub query_timeout: Option<i64>,

    #[serde(rename = "AnalyticsQueryTimeout")]
    pub analytics_query_timeout: Option<i64>,

    #[serde(rename = "DisableDatabaseSearch")]
    pub disable_database_search: Option<bool>,

    #[serde(rename = "MigrationsStatementTimeoutSeconds")]
    pub migrations_statement_timeout_seconds: Option<i64>,

    #[serde(rename = "ReplicaLagSettings")]
    pub replica_lag_settings: Option<Vec<ReplicaLagSettings>>,

    #[serde(rename = "ReplicaMonitorIntervalSeconds")]
    pub replica_monitor_interval_seconds: Option<i64>,
}

/// Port of `model.LogSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogSettings {
    #[serde(rename = "EnableConsole")]
    pub enable_console: Option<bool>,

    #[serde(rename = "ConsoleLevel")]
    pub console_level: Option<String>,

    #[serde(rename = "ConsoleJson")]
    pub console_json: Option<bool>,

    #[serde(rename = "EnableColor")]
    pub enable_color: Option<bool>,

    #[serde(rename = "EnableFile")]
    pub enable_file: Option<bool>,

    #[serde(rename = "FileLevel")]
    pub file_level: Option<String>,

    #[serde(rename = "FileJson")]
    pub file_json: Option<bool>,

    #[serde(rename = "FileLocation")]
    pub file_location: Option<String>,

    #[serde(rename = "EnableWebhookDebugging")]
    pub enable_webhook_debugging: Option<bool>,

    #[serde(rename = "EnableDiagnostics")]
    pub enable_diagnostics: Option<bool>,

    #[serde(rename = "EnableSentry")]
    pub enable_sentry: Option<bool>,

    #[serde(rename = "AdvancedLoggingJSON")]
    pub advanced_logging_json: serde_json::Value,

    #[serde(rename = "MaxFieldSize")]
    pub max_field_size: Option<i64>,
}

/// Port of `model.ExperimentalAuditSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExperimentalAuditSettings {
    #[serde(rename = "FileEnabled")]
    pub file_enabled: Option<bool>,

    #[serde(rename = "FileName")]
    pub file_name: Option<String>,

    #[serde(rename = "AdvancedLoggingJSON")]
    pub advanced_logging_json: serde_json::Value,

    #[serde(rename = "Certificate")]
    pub certificate: Option<String>,
}

/// Port of `model.PasswordSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PasswordSettings {
    #[serde(rename = "MinimumLength")]
    pub minimum_length: Option<i64>,

    #[serde(rename = "Lowercase")]
    pub lowercase: Option<bool>,

    #[serde(rename = "Number")]
    pub number: Option<bool>,

    #[serde(rename = "Uppercase")]
    pub uppercase: Option<bool>,

    #[serde(rename = "Symbol")]
    pub symbol: Option<bool>,

    #[serde(rename = "EnableForgotLink")]
    pub enable_forgot_link: Option<bool>,
}

/// Port of `model.FileSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileSettings {
    #[serde(rename = "EnableFileAttachments")]
    pub enable_file_attachments: Option<bool>,

    #[serde(rename = "EnableMobileUpload")]
    pub enable_mobile_upload: Option<bool>,

    #[serde(rename = "EnableMobileDownload")]
    pub enable_mobile_download: Option<bool>,

    #[serde(rename = "MaxFileSize")]
    pub max_file_size: Option<i64>,

    #[serde(rename = "MaxImageResolution")]
    pub max_image_resolution: Option<i64>,

    #[serde(rename = "MaxImageDecoderConcurrency")]
    pub max_image_decoder_concurrency: Option<i64>,

    #[serde(rename = "DriverName")]
    pub driver_name: Option<String>,

    #[serde(rename = "Directory")]
    pub directory: Option<String>,

    #[serde(rename = "EnablePublicLink")]
    pub enable_public_link: Option<bool>,

    #[serde(rename = "ExtractContent")]
    pub extract_content: Option<bool>,

    #[serde(rename = "ExtractContentTimeout")]
    pub extract_content_timeout: Option<i64>,

    #[serde(rename = "ArchiveRecursion")]
    pub archive_recursion: Option<bool>,

    #[serde(rename = "PublicLinkSalt")]
    pub public_link_salt: Option<String>,

    #[serde(rename = "InitialFont")]
    pub initial_font: Option<String>,

    #[serde(rename = "AmazonS3AccessKeyId")]
    pub amazon_s3_access_key_id: Option<String>,

    #[serde(rename = "AmazonS3SecretAccessKey")]
    pub amazon_s3_secret_access_key: Option<String>,

    #[serde(rename = "AmazonS3Bucket")]
    pub amazon_s3_bucket: Option<String>,

    #[serde(rename = "AmazonS3PathPrefix")]
    pub amazon_s3_path_prefix: Option<String>,

    #[serde(rename = "AmazonS3Region")]
    pub amazon_s3_region: Option<String>,

    #[serde(rename = "AmazonS3Endpoint")]
    pub amazon_s3_endpoint: Option<String>,

    #[serde(rename = "AmazonS3SSL")]
    pub amazon_s3_ssl: Option<bool>,

    #[serde(rename = "AmazonS3SignV2")]
    pub amazon_s3_sign_v2: Option<bool>,

    #[serde(rename = "AmazonS3SSE")]
    pub amazon_s3_sse: Option<bool>,

    #[serde(rename = "AmazonS3Trace")]
    pub amazon_s3_trace: Option<bool>,

    #[serde(rename = "AmazonS3RequestTimeoutMilliseconds")]
    pub amazon_s3_request_timeout_milliseconds: Option<i64>,

    #[serde(rename = "AmazonS3UploadPartSizeBytes")]
    pub amazon_s3_upload_part_size_bytes: Option<i64>,

    #[serde(rename = "AmazonS3StorageClass")]
    pub amazon_s3_storage_class: Option<String>,

    #[serde(rename = "AzureStorageAccount")]
    pub azure_storage_account: Option<String>,

    #[serde(rename = "AzureAuthMode")]
    pub azure_auth_mode: Option<String>,

    #[serde(rename = "AzureAccessKey")]
    pub azure_access_key: Option<String>,

    #[serde(rename = "AzureContainer")]
    pub azure_container: Option<String>,

    #[serde(rename = "AzurePathPrefix")]
    pub azure_path_prefix: Option<String>,

    #[serde(rename = "AzureCloud")]
    pub azure_cloud: Option<String>,

    #[serde(rename = "AzureEndpoint")]
    pub azure_endpoint: Option<String>,

    #[serde(rename = "AzureSSL")]
    pub azure_ssl: Option<bool>,

    #[serde(rename = "AzureRequestTimeoutMilliseconds")]
    pub azure_request_timeout_milliseconds: Option<i64>,

    #[serde(rename = "DedicatedExportStore")]
    pub dedicated_export_store: Option<bool>,

    #[serde(rename = "ExportDriverName")]
    pub export_driver_name: Option<String>,

    #[serde(rename = "ExportDirectory")]
    pub export_directory: Option<String>,

    #[serde(rename = "ExportAmazonS3AccessKeyId")]
    pub export_amazon_s3_access_key_id: Option<String>,

    #[serde(rename = "ExportAmazonS3SecretAccessKey")]
    pub export_amazon_s3_secret_access_key: Option<String>,

    #[serde(rename = "ExportAmazonS3Bucket")]
    pub export_amazon_s3_bucket: Option<String>,

    #[serde(rename = "ExportAmazonS3PathPrefix")]
    pub export_amazon_s3_path_prefix: Option<String>,

    #[serde(rename = "ExportAmazonS3Region")]
    pub export_amazon_s3_region: Option<String>,

    #[serde(rename = "ExportAmazonS3Endpoint")]
    pub export_amazon_s3_endpoint: Option<String>,

    #[serde(rename = "ExportAmazonS3SSL")]
    pub export_amazon_s3_ssl: Option<bool>,

    #[serde(rename = "ExportAmazonS3SignV2")]
    pub export_amazon_s3_sign_v2: Option<bool>,

    #[serde(rename = "ExportAmazonS3SSE")]
    pub export_amazon_s3_sse: Option<bool>,

    #[serde(rename = "ExportAmazonS3Trace")]
    pub export_amazon_s3_trace: Option<bool>,

    #[serde(rename = "ExportAmazonS3RequestTimeoutMilliseconds")]
    pub export_amazon_s3_request_timeout_milliseconds: Option<i64>,

    #[serde(rename = "ExportAmazonS3PresignExpiresSeconds")]
    pub export_amazon_s3_presign_expires_seconds: Option<i64>,

    #[serde(rename = "ExportAmazonS3UploadPartSizeBytes")]
    pub export_amazon_s3_upload_part_size_bytes: Option<i64>,

    #[serde(rename = "ExportAmazonS3StorageClass")]
    pub export_amazon_s3_storage_class: Option<String>,

    #[serde(rename = "ExportAzureStorageAccount")]
    pub export_azure_storage_account: Option<String>,

    #[serde(rename = "ExportAzureAuthMode")]
    pub export_azure_auth_mode: Option<String>,

    #[serde(rename = "ExportAzureAccessKey")]
    pub export_azure_access_key: Option<String>,

    #[serde(rename = "ExportAzureContainer")]
    pub export_azure_container: Option<String>,

    #[serde(rename = "ExportAzurePathPrefix")]
    pub export_azure_path_prefix: Option<String>,

    #[serde(rename = "ExportAzureCloud")]
    pub export_azure_cloud: Option<String>,

    #[serde(rename = "ExportAzureEndpoint")]
    pub export_azure_endpoint: Option<String>,

    #[serde(rename = "ExportAzureSSL")]
    pub export_azure_ssl: Option<bool>,

    #[serde(rename = "ExportAzureRequestTimeoutMilliseconds")]
    pub export_azure_request_timeout_milliseconds: Option<i64>,

    #[serde(rename = "ExportAzurePresignExpiresSeconds")]
    pub export_azure_presign_expires_seconds: Option<i64>,
}

/// Port of `model.EmailSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmailSettings {
    #[serde(rename = "EnableSignUpWithEmail")]
    pub enable_sign_up_with_email: Option<bool>,

    #[serde(rename = "EnableSignInWithEmail")]
    pub enable_sign_in_with_email: Option<bool>,

    #[serde(rename = "EnableSignInWithUsername")]
    pub enable_sign_in_with_username: Option<bool>,

    #[serde(rename = "SendEmailNotifications")]
    pub send_email_notifications: Option<bool>,

    #[serde(rename = "UseChannelInEmailNotifications")]
    pub use_channel_in_email_notifications: Option<bool>,

    #[serde(rename = "RequireEmailVerification")]
    pub require_email_verification: Option<bool>,

    #[serde(rename = "FeedbackName")]
    pub feedback_name: Option<String>,

    #[serde(rename = "FeedbackEmail")]
    pub feedback_email: Option<String>,

    #[serde(rename = "ReplyToAddress")]
    pub reply_to_address: Option<String>,

    #[serde(rename = "FeedbackOrganization")]
    pub feedback_organization: Option<String>,

    #[serde(rename = "EnableSMTPAuth")]
    pub enable_smtp_auth: Option<bool>,

    #[serde(rename = "SMTPUsername")]
    pub smtp_username: Option<String>,

    #[serde(rename = "SMTPPassword")]
    pub smtp_password: Option<String>,

    #[serde(rename = "SMTPServer")]
    pub smtp_server: Option<String>,

    #[serde(rename = "SMTPPort")]
    pub smtp_port: Option<String>,

    #[serde(rename = "SMTPServerTimeout")]
    pub smtp_server_timeout: Option<i64>,

    #[serde(rename = "ConnectionSecurity")]
    pub connection_security: Option<String>,

    #[serde(rename = "SendPushNotifications")]
    pub send_push_notifications: Option<bool>,

    #[serde(rename = "PushNotificationServer")]
    pub push_notification_server: Option<String>,

    #[serde(rename = "PushNotificationContents")]
    pub push_notification_contents: Option<String>,

    #[serde(rename = "PushNotificationBuffer")]
    pub push_notification_buffer: Option<i64>,

    #[serde(rename = "EnableEmailBatching")]
    pub enable_email_batching: Option<bool>,

    #[serde(rename = "EmailBatchingBufferSize")]
    pub email_batching_buffer_size: Option<i64>,

    #[serde(rename = "EmailBatchingInterval")]
    pub email_batching_interval: Option<i64>,

    #[serde(rename = "EnablePreviewModeBanner")]
    pub enable_preview_mode_banner: Option<bool>,

    #[serde(rename = "SkipServerCertificateVerification")]
    pub skip_server_certificate_verification: Option<bool>,

    #[serde(rename = "EmailNotificationContentsType")]
    pub email_notification_contents_type: Option<String>,

    #[serde(rename = "LoginButtonColor")]
    pub login_button_color: Option<String>,

    #[serde(rename = "LoginButtonBorderColor")]
    pub login_button_border_color: Option<String>,

    #[serde(rename = "LoginButtonTextColor")]
    pub login_button_text_color: Option<String>,
}

/// Port of `model.RateLimitSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimitSettings {
    #[serde(rename = "Enable")]
    pub enable: Option<bool>,

    #[serde(rename = "PerSec")]
    pub per_sec: Option<i64>,

    #[serde(rename = "MaxBurst")]
    pub max_burst: Option<i64>,

    #[serde(rename = "MemoryStoreSize")]
    pub memory_store_size: Option<i64>,

    #[serde(rename = "VaryByRemoteAddr")]
    pub vary_by_remote_addr: Option<bool>,

    #[serde(rename = "VaryByUser")]
    pub vary_by_user: Option<bool>,

    #[serde(rename = "VaryByHeader")]
    pub vary_by_header: String,
}

/// Port of `model.PrivacySettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrivacySettings {
    #[serde(rename = "ShowEmailAddress")]
    pub show_email_address: Option<bool>,

    #[serde(rename = "ShowFullName")]
    pub show_full_name: Option<bool>,

    #[serde(rename = "UseAnonymousURLs")]
    pub use_anonymous_ur_ls: Option<bool>,
}

/// Port of `model.SupportSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SupportSettings {
    #[serde(rename = "TermsOfServiceLink")]
    pub terms_of_service_link: Option<String>,

    #[serde(rename = "PrivacyPolicyLink")]
    pub privacy_policy_link: Option<String>,

    #[serde(rename = "AboutLink")]
    pub about_link: Option<String>,

    #[serde(rename = "HelpLink")]
    pub help_link: Option<String>,

    #[serde(rename = "ReportAProblemLink")]
    pub report_a_problem_link: Option<String>,

    #[serde(rename = "ReportAProblemType")]
    pub report_a_problem_type: Option<String>,

    #[serde(rename = "ReportAProblemMail")]
    pub report_a_problem_mail: Option<String>,

    #[serde(rename = "AllowDownloadLogs")]
    pub allow_download_logs: Option<bool>,

    #[serde(rename = "ForgotPasswordLink")]
    pub forgot_password_link: Option<String>,

    #[serde(rename = "SupportEmail")]
    pub support_email: Option<String>,

    #[serde(rename = "CustomTermsOfServiceEnabled")]
    pub custom_terms_of_service_enabled: Option<bool>,

    #[serde(rename = "CustomTermsOfServiceReAcceptancePeriod")]
    pub custom_terms_of_service_re_acceptance_period: Option<i64>,

    #[serde(rename = "EnableAskCommunityLink")]
    pub enable_ask_community_link: Option<bool>,
}

/// Port of `model.AnnouncementSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnnouncementSettings {
    #[serde(rename = "EnableBanner")]
    pub enable_banner: Option<bool>,

    #[serde(rename = "BannerText")]
    pub banner_text: Option<String>,

    #[serde(rename = "BannerColor")]
    pub banner_color: Option<String>,

    #[serde(rename = "BannerTextColor")]
    pub banner_text_color: Option<String>,

    #[serde(rename = "AllowBannerDismissal")]
    pub allow_banner_dismissal: Option<bool>,

    #[serde(rename = "AdminNoticesEnabled")]
    pub admin_notices_enabled: Option<bool>,

    #[serde(rename = "UserNoticesEnabled")]
    pub user_notices_enabled: Option<bool>,

    #[serde(rename = "NoticesURL")]
    pub notices_url: Option<String>,

    #[serde(rename = "NoticesFetchFrequency")]
    pub notices_fetch_frequency: Option<i64>,

    #[serde(rename = "NoticesSkipCache")]
    pub notices_skip_cache: Option<bool>,
}

/// Port of `model.ThemeSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeSettings {
    #[serde(rename = "EnableThemeSelection")]
    pub enable_theme_selection: Option<bool>,

    #[serde(rename = "DefaultTheme")]
    pub default_theme: Option<String>,

    #[serde(rename = "AllowCustomThemes")]
    pub allow_custom_themes: Option<bool>,

    #[serde(rename = "AllowedThemes")]
    pub allowed_themes: Option<Vec<String>>,
}

/// Port of `model.TeamSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TeamSettings {
    #[serde(rename = "SiteName")]
    pub site_name: Option<String>,

    #[serde(rename = "MaxUsersPerTeam")]
    pub max_users_per_team: Option<i64>,

    #[serde(rename = "EnableJoinLeaveMessageByDefault")]
    pub enable_join_leave_message_by_default: Option<bool>,

    #[serde(rename = "EnableUserCreation")]
    pub enable_user_creation: Option<bool>,

    #[serde(rename = "EnableOpenServer")]
    pub enable_open_server: Option<bool>,

    #[serde(rename = "EnableUserDeactivation")]
    pub enable_user_deactivation: Option<bool>,

    #[serde(rename = "RestrictCreationToDomains")]
    pub restrict_creation_to_domains: Option<String>,

    #[serde(rename = "EnableCustomUserStatuses")]
    pub enable_custom_user_statuses: Option<bool>,

    #[serde(rename = "EnableCustomBrand")]
    pub enable_custom_brand: Option<bool>,

    #[serde(rename = "CustomBrandText")]
    pub custom_brand_text: Option<String>,

    #[serde(rename = "CustomDescriptionText")]
    pub custom_description_text: Option<String>,

    #[serde(rename = "RestrictDirectMessage")]
    pub restrict_direct_message: Option<String>,

    #[serde(rename = "EnableLastActiveTime")]
    pub enable_last_active_time: Option<bool>,

    #[serde(rename = "UserStatusAwayTimeout")]
    pub user_status_away_timeout: Option<i64>,

    #[serde(rename = "MaxChannelsPerTeam")]
    pub max_channels_per_team: Option<i64>,

    #[serde(rename = "EnableChannelCategorySorting")]
    pub enable_channel_category_sorting: Option<bool>,

    #[serde(rename = "MaxNotificationsPerChannel")]
    pub max_notifications_per_channel: Option<i64>,

    #[serde(rename = "EnableConfirmNotificationsToChannel")]
    pub enable_confirm_notifications_to_channel: Option<bool>,

    #[serde(rename = "TeammateNameDisplay")]
    pub teammate_name_display: Option<String>,

    #[serde(rename = "ExperimentalViewArchivedChannels")]
    pub experimental_view_archived_channels: Option<bool>,

    #[serde(rename = "ExperimentalEnableAutomaticReplies")]
    pub experimental_enable_automatic_replies: Option<bool>,

    #[serde(rename = "LockTeammateNameDisplay")]
    pub lock_teammate_name_display: Option<bool>,

    #[serde(rename = "LockProfileFieldsForEmailUsers")]
    pub lock_profile_fields_for_email_users: Option<String>,

    #[serde(rename = "ExperimentalPrimaryTeam")]
    pub experimental_primary_team: Option<String>,

    #[serde(rename = "ExperimentalDefaultChannels")]
    pub experimental_default_channels: Option<Vec<String>>,
}

/// Port of `model.ClientRequirements` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientRequirements {
    #[serde(rename = "AndroidLatestVersion")]
    pub android_latest_version: String,

    #[serde(rename = "AndroidMinVersion")]
    pub android_min_version: String,

    #[serde(rename = "IosLatestVersion")]
    pub ios_latest_version: String,

    #[serde(rename = "IosMinVersion")]
    pub ios_min_version: String,
}

/// Port of `model.LdapSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LdapSettings {
    #[serde(rename = "Enable")]
    pub enable: Option<bool>,

    #[serde(rename = "EnableSync")]
    pub enable_sync: Option<bool>,

    #[serde(rename = "LdapServer")]
    pub ldap_server: Option<String>,

    #[serde(rename = "LdapPort")]
    pub ldap_port: Option<i64>,

    #[serde(rename = "ConnectionSecurity")]
    pub connection_security: Option<String>,

    #[serde(rename = "BaseDN")]
    pub base_dn: Option<String>,

    #[serde(rename = "BindUsername")]
    pub bind_username: Option<String>,

    #[serde(rename = "BindPassword")]
    pub bind_password: Option<String>,

    #[serde(rename = "MaximumLoginAttempts")]
    pub maximum_login_attempts: Option<i64>,

    #[serde(rename = "UserFilter")]
    pub user_filter: Option<String>,

    #[serde(rename = "GroupFilter")]
    pub group_filter: Option<String>,

    #[serde(rename = "GuestFilter")]
    pub guest_filter: Option<String>,

    #[serde(rename = "EnableAdminFilter")]
    pub enable_admin_filter: Option<bool>,

    #[serde(rename = "AdminFilter")]
    pub admin_filter: Option<String>,

    #[serde(rename = "GroupDisplayNameAttribute")]
    pub group_display_name_attribute: Option<String>,

    #[serde(rename = "GroupIdAttribute")]
    pub group_id_attribute: Option<String>,

    #[serde(rename = "FirstNameAttribute")]
    pub first_name_attribute: Option<String>,

    #[serde(rename = "LastNameAttribute")]
    pub last_name_attribute: Option<String>,

    #[serde(rename = "EmailAttribute")]
    pub email_attribute: Option<String>,

    #[serde(rename = "UsernameAttribute")]
    pub username_attribute: Option<String>,

    #[serde(rename = "NicknameAttribute")]
    pub nickname_attribute: Option<String>,

    #[serde(rename = "IdAttribute")]
    pub id_attribute: Option<String>,

    #[serde(rename = "PositionAttribute")]
    pub position_attribute: Option<String>,

    #[serde(rename = "LoginIdAttribute")]
    pub login_id_attribute: Option<String>,

    #[serde(rename = "PictureAttribute")]
    pub picture_attribute: Option<String>,

    #[serde(rename = "SyncIntervalMinutes")]
    pub sync_interval_minutes: Option<i64>,

    #[serde(rename = "ReAddRemovedMembers")]
    pub re_add_removed_members: Option<bool>,

    #[serde(rename = "SkipCertificateVerification")]
    pub skip_certificate_verification: Option<bool>,

    #[serde(rename = "PublicCertificateFile")]
    pub public_certificate_file: Option<String>,

    #[serde(rename = "PrivateKeyFile")]
    pub private_key_file: Option<String>,

    #[serde(rename = "QueryTimeout")]
    pub query_timeout: Option<i64>,

    #[serde(rename = "MaxPageSize")]
    pub max_page_size: Option<i64>,

    #[serde(rename = "LoginFieldName")]
    pub login_field_name: Option<String>,
}

/// Port of `model.ComplianceSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ComplianceSettings {
    #[serde(rename = "Enable")]
    pub enable: Option<bool>,

    #[serde(rename = "Directory")]
    pub directory: Option<String>,

    #[serde(rename = "EnableDaily")]
    pub enable_daily: Option<bool>,

    #[serde(rename = "BatchSize")]
    pub batch_size: Option<i64>,
}

/// Port of `model.LocalizationSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalizationSettings {
    #[serde(rename = "DefaultServerLocale")]
    pub default_server_locale: Option<String>,

    #[serde(rename = "DefaultClientLocale")]
    pub default_client_locale: Option<String>,

    #[serde(rename = "AvailableLocales")]
    pub available_locales: Option<String>,

    #[serde(rename = "EnableExperimentalLocales")]
    pub enable_experimental_locales: Option<bool>,
}

/// Port of `model.AutoTranslationSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoTranslationSettings {
    #[serde(rename = "Enable")]
    pub enable: Option<bool>,

    #[serde(rename = "RestrictDMAndGM")]
    pub restrict_dm_and_gm: Option<bool>,

    #[serde(rename = "Provider")]
    pub provider: Option<String>,

    #[serde(rename = "TargetLanguages")]
    pub target_languages: Option<Vec<String>>,

    #[serde(rename = "Workers")]
    pub workers: Option<i64>,

    #[serde(rename = "TimeoutMs")]
    pub timeout_ms: Option<i64>,

    #[serde(rename = "LibreTranslate")]
    pub libre_translate: Option<LibreTranslateProviderSettings>,

    #[serde(rename = "Agents")]
    pub agents: Option<AgentsProviderSettings>,
}

/// Port of `model.LibreTranslateProviderSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LibreTranslateProviderSettings {
    #[serde(rename = "URL")]
    pub url: Option<String>,

    #[serde(rename = "APIKey")]
    pub api_key: Option<String>,
}

/// Port of `model.AgentsProviderSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentsProviderSettings {
    #[serde(rename = "LLMServiceID")]
    pub llm_service_id: Option<String>,
}

/// Port of `model.SamlSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SamlSettings {
    #[serde(rename = "Enable")]
    pub enable: Option<bool>,

    #[serde(rename = "EnableSyncWithLdap")]
    pub enable_sync_with_ldap: Option<bool>,

    #[serde(rename = "EnableSyncWithLdapIncludeAuth")]
    pub enable_sync_with_ldap_include_auth: Option<bool>,

    #[serde(rename = "IgnoreGuestsLdapSync")]
    pub ignore_guests_ldap_sync: Option<bool>,

    #[serde(rename = "Verify")]
    pub verify: Option<bool>,

    #[serde(rename = "Encrypt")]
    pub encrypt: Option<bool>,

    #[serde(rename = "SignRequest")]
    pub sign_request: Option<bool>,

    #[serde(rename = "IdpURL")]
    pub idp_url: Option<String>,

    #[serde(rename = "IdpDescriptorURL")]
    pub idp_descriptor_url: Option<String>,

    #[serde(rename = "IdpMetadataURL")]
    pub idp_metadata_url: Option<String>,

    #[serde(rename = "ServiceProviderIdentifier")]
    pub service_provider_identifier: Option<String>,

    #[serde(rename = "AssertionConsumerServiceURL")]
    pub assertion_consumer_service_url: Option<String>,

    #[serde(rename = "SignatureAlgorithm")]
    pub signature_algorithm: Option<String>,

    #[serde(rename = "CanonicalAlgorithm")]
    pub canonical_algorithm: Option<String>,

    #[serde(rename = "ScopingIDPProviderId")]
    pub scoping_idp_provider_id: Option<String>,

    #[serde(rename = "ScopingIDPName")]
    pub scoping_idp_name: Option<String>,

    #[serde(rename = "IdpCertificateFile")]
    pub idp_certificate_file: Option<String>,

    #[serde(rename = "PublicCertificateFile")]
    pub public_certificate_file: Option<String>,

    #[serde(rename = "PrivateKeyFile")]
    pub private_key_file: Option<String>,

    #[serde(rename = "IdAttribute")]
    pub id_attribute: Option<String>,

    #[serde(rename = "GuestAttribute")]
    pub guest_attribute: Option<String>,

    #[serde(rename = "EnableAdminAttribute")]
    pub enable_admin_attribute: Option<bool>,

    #[serde(rename = "AdminAttribute")]
    pub admin_attribute: Option<String>,

    #[serde(rename = "FirstNameAttribute")]
    pub first_name_attribute: Option<String>,

    #[serde(rename = "LastNameAttribute")]
    pub last_name_attribute: Option<String>,

    #[serde(rename = "EmailAttribute")]
    pub email_attribute: Option<String>,

    #[serde(rename = "UsernameAttribute")]
    pub username_attribute: Option<String>,

    #[serde(rename = "NicknameAttribute")]
    pub nickname_attribute: Option<String>,

    #[serde(rename = "LocaleAttribute")]
    pub locale_attribute: Option<String>,

    #[serde(rename = "PositionAttribute")]
    pub position_attribute: Option<String>,

    #[serde(rename = "LoginButtonText")]
    pub login_button_text: Option<String>,
}

/// Port of `model.NativeAppSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NativeAppSettings {
    #[serde(rename = "AppCustomURLSchemes")]
    pub app_custom_url_schemes: Option<Vec<String>>,

    #[serde(rename = "AppDownloadLink")]
    pub app_download_link: Option<String>,

    #[serde(rename = "AndroidAppDownloadLink")]
    pub android_app_download_link: Option<String>,

    #[serde(rename = "IosAppDownloadLink")]
    pub ios_app_download_link: Option<String>,

    #[serde(rename = "MobileExternalBrowser")]
    pub mobile_external_browser: Option<bool>,

    #[serde(rename = "MobileEnableBiometrics")]
    pub mobile_enable_biometrics: Option<bool>,

    #[serde(rename = "MobilePreventScreenCapture")]
    pub mobile_prevent_screen_capture: Option<bool>,

    #[serde(rename = "MobileJailbreakProtection")]
    pub mobile_jailbreak_protection: Option<bool>,

    #[serde(rename = "MobileEnableSecureFilePreview")]
    pub mobile_enable_secure_file_preview: Option<bool>,

    #[serde(rename = "MobileAllowPdfLinkNavigation")]
    pub mobile_allow_pdf_link_navigation: Option<bool>,

    #[serde(rename = "EnableIntuneMAM")]
    pub enable_intune_mam: Option<bool>,
}

/// Port of `model.ElasticsearchSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ElasticsearchSettings {
    #[serde(rename = "ConnectionURL")]
    pub connection_url: Option<String>,

    #[serde(rename = "Backend")]
    pub backend: Option<String>,

    #[serde(rename = "Username")]
    pub username: Option<String>,

    #[serde(rename = "Password")]
    pub password: Option<String>,

    #[serde(rename = "EnableIndexing")]
    pub enable_indexing: Option<bool>,

    #[serde(rename = "EnableSearching")]
    pub enable_searching: Option<bool>,

    #[serde(rename = "EnableCJKAnalyzers")]
    pub enable_cjk_analyzers: Option<bool>,

    #[serde(rename = "EnableAutocomplete")]
    pub enable_autocomplete: Option<bool>,

    #[serde(rename = "Sniff")]
    pub sniff: Option<bool>,

    #[serde(rename = "PostIndexReplicas")]
    pub post_index_replicas: Option<i64>,

    #[serde(rename = "PostIndexShards")]
    pub post_index_shards: Option<i64>,

    #[serde(rename = "ChannelIndexReplicas")]
    pub channel_index_replicas: Option<i64>,

    #[serde(rename = "ChannelIndexShards")]
    pub channel_index_shards: Option<i64>,

    #[serde(rename = "UserIndexReplicas")]
    pub user_index_replicas: Option<i64>,

    #[serde(rename = "UserIndexShards")]
    pub user_index_shards: Option<i64>,

    #[serde(rename = "AggregatePostsAfterDays")]
    pub aggregate_posts_after_days: Option<i64>,

    #[serde(rename = "PostsAggregatorJobStartTime")]
    pub posts_aggregator_job_start_time: Option<String>,

    #[serde(rename = "IndexPrefix")]
    pub index_prefix: Option<String>,

    #[serde(rename = "GlobalSearchPrefix")]
    pub global_search_prefix: Option<String>,

    #[serde(rename = "LiveIndexingBatchSize")]
    pub live_indexing_batch_size: Option<i64>,

    #[serde(
        rename = "BulkIndexingTimeWindowSeconds",
        skip_serializing_if = "Option::is_none"
    )]
    pub bulk_indexing_time_window_seconds: Option<i64>,

    #[serde(rename = "BatchSize")]
    pub batch_size: Option<i64>,

    #[serde(rename = "RequestTimeoutSeconds")]
    pub request_timeout_seconds: Option<i64>,

    #[serde(rename = "SkipTLSVerification")]
    pub skip_tls_verification: Option<bool>,

    #[serde(rename = "CA")]
    pub ca: Option<String>,

    #[serde(rename = "ClientCert")]
    pub client_cert: Option<String>,

    #[serde(rename = "ClientKey")]
    pub client_key: Option<String>,

    #[serde(rename = "Trace")]
    pub trace: Option<String>,

    #[serde(rename = "IgnoredPurgeIndexes")]
    pub ignored_purge_indexes: Option<String>,

    #[serde(rename = "EnableSearchPublicChannelsWithoutMembership")]
    pub enable_search_public_channels_without_membership: Option<bool>,
}

/// Port of `model.DataRetentionSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DataRetentionSettings {
    #[serde(rename = "EnableMessageDeletion")]
    pub enable_message_deletion: Option<bool>,

    #[serde(rename = "EnableFileDeletion")]
    pub enable_file_deletion: Option<bool>,

    #[serde(rename = "EnableBoardsDeletion")]
    pub enable_boards_deletion: Option<bool>,

    #[serde(rename = "MessageRetentionDays")]
    pub message_retention_days: Option<i64>,

    #[serde(rename = "MessageRetentionHours")]
    pub message_retention_hours: Option<i64>,

    #[serde(rename = "FileRetentionDays")]
    pub file_retention_days: Option<i64>,

    #[serde(rename = "FileRetentionHours")]
    pub file_retention_hours: Option<i64>,

    #[serde(rename = "BoardsRetentionDays")]
    pub boards_retention_days: Option<i64>,

    #[serde(rename = "DeletionJobStartTime")]
    pub deletion_job_start_time: Option<String>,

    #[serde(rename = "BatchSize")]
    pub batch_size: Option<i64>,

    #[serde(rename = "TimeBetweenBatchesMilliseconds")]
    pub time_between_batches_milliseconds: Option<i64>,

    #[serde(rename = "RetentionIdsBatchSize")]
    pub retention_ids_batch_size: Option<i64>,

    #[serde(rename = "PreservePinnedPosts")]
    pub preserve_pinned_posts: Option<bool>,
}

/// Port of `model.MobileEphemeralModeSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MobileEphemeralModeSettings {
    #[serde(rename = "Enable")]
    pub enable: Option<bool>,

    #[serde(rename = "DisconnectionTimeoutSeconds")]
    pub disconnection_timeout_seconds: Option<i64>,

    #[serde(rename = "OfflinePersistenceTimerHours")]
    pub offline_persistence_timer_hours: Option<i64>,

    #[serde(rename = "AutoCacheCleanupDays")]
    pub auto_cache_cleanup_days: Option<i64>,
}

/// Port of `model.JobSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobSettings {
    #[serde(rename = "RunJobs")]
    pub run_jobs: Option<bool>,

    #[serde(rename = "RunScheduler")]
    pub run_scheduler: Option<bool>,

    #[serde(rename = "CleanupJobsThresholdDays")]
    pub cleanup_jobs_threshold_days: Option<i64>,

    #[serde(rename = "CleanupConfigThresholdDays")]
    pub cleanup_config_threshold_days: Option<i64>,
}

/// Port of `model.CloudSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudSettings {
    #[serde(rename = "CWSURL")]
    pub cwsurl: Option<String>,

    #[serde(rename = "CWSAPIURL")]
    pub cwsapiurl: Option<String>,

    #[serde(rename = "CWSMock")]
    pub cws_mock: Option<bool>,

    #[serde(rename = "Disable")]
    pub disable: Option<bool>,

    #[serde(rename = "PreviewModalBucketURL")]
    pub preview_modal_bucket_url: Option<String>,
}

/// Port of `model.PluginState` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginState {
    #[serde(rename = "Enable")]
    pub enable: bool,
}

/// Port of `model.PluginSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginSettings {
    #[serde(rename = "Enable")]
    pub enable: Option<bool>,

    #[serde(rename = "EnableUploads")]
    pub enable_uploads: Option<bool>,

    #[serde(rename = "AllowInsecureDownloadURL")]
    pub allow_insecure_download_url: Option<bool>,

    #[serde(rename = "EnableHealthCheck")]
    pub enable_health_check: Option<bool>,

    #[serde(rename = "Directory")]
    pub directory: Option<String>,

    #[serde(rename = "ClientDirectory")]
    pub client_directory: Option<String>,

    #[serde(rename = "Plugins")]
    pub plugins: Option<std::collections::BTreeMap<String, StringInterface>>,

    #[serde(rename = "PluginStates")]
    pub plugin_states: Option<std::collections::BTreeMap<String, PluginState>>,

    #[serde(rename = "EnableMarketplace")]
    pub enable_marketplace: Option<bool>,

    #[serde(rename = "EnableRemoteMarketplace")]
    pub enable_remote_marketplace: Option<bool>,

    #[serde(rename = "AutomaticPrepackagedPlugins")]
    pub automatic_prepackaged_plugins: Option<bool>,

    #[serde(rename = "RequirePluginSignature")]
    pub require_plugin_signature: Option<bool>,

    #[serde(rename = "MarketplaceURL")]
    pub marketplace_url: Option<String>,

    #[serde(rename = "SignaturePublicKeyFiles")]
    pub signature_public_key_files: Option<Vec<String>>,

    #[serde(rename = "ChimeraOAuthProxyURL")]
    pub chimera_o_auth_proxy_url: Option<String>,
}

/// Port of `model.WranglerSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WranglerSettings {
    #[serde(rename = "PermittedWranglerRoles")]
    pub permitted_wrangler_roles: Option<Vec<String>>,

    #[serde(rename = "AllowedEmailDomain")]
    pub allowed_email_domain: Option<Vec<String>>,

    #[serde(rename = "MoveThreadMaxCount")]
    pub move_thread_max_count: Option<i64>,

    #[serde(rename = "MoveThreadToAnotherTeamEnable")]
    pub move_thread_to_another_team_enable: Option<bool>,

    #[serde(rename = "MoveThreadFromPrivateChannelEnable")]
    pub move_thread_from_private_channel_enable: Option<bool>,

    #[serde(rename = "MoveThreadFromDirectMessageChannelEnable")]
    pub move_thread_from_direct_message_channel_enable: Option<bool>,

    #[serde(rename = "MoveThreadFromGroupMessageChannelEnable")]
    pub move_thread_from_group_message_channel_enable: Option<bool>,
}

/// Port of `model.ConnectedWorkspacesSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectedWorkspacesSettings {
    #[serde(rename = "EnableSharedChannels")]
    pub enable_shared_channels: Option<bool>,

    #[serde(rename = "EnableRemoteClusterService")]
    pub enable_remote_cluster_service: Option<bool>,

    #[serde(rename = "DisableSharedChannelsStatusSync")]
    pub disable_shared_channels_status_sync: Option<bool>,

    #[serde(rename = "SyncUsersOnConnectionOpen")]
    pub sync_users_on_connection_open: Option<bool>,

    #[serde(rename = "GlobalUserSyncBatchSize")]
    pub global_user_sync_batch_size: Option<i64>,

    #[serde(rename = "MaxPostsPerSync")]
    pub max_posts_per_sync: Option<i64>,

    #[serde(rename = "MemberSyncBatchSize")]
    pub member_sync_batch_size: Option<i64>,
}

/// Port of `model.GlobalRelayMessageExportSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalRelayMessageExportSettings {
    #[serde(rename = "CustomerType")]
    pub customer_type: Option<String>,

    #[serde(rename = "SMTPUsername")]
    pub smtp_username: Option<String>,

    #[serde(rename = "SMTPPassword")]
    pub smtp_password: Option<String>,

    #[serde(rename = "EmailAddress")]
    pub email_address: Option<String>,

    #[serde(rename = "SMTPServerTimeout")]
    pub smtp_server_timeout: Option<i64>,

    #[serde(rename = "CustomSMTPServerName")]
    pub custom_smtp_server_name: Option<String>,

    #[serde(rename = "CustomSMTPPort")]
    pub custom_smtp_port: Option<String>,
}

/// Port of `model.MessageExportSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MessageExportSettings {
    #[serde(rename = "EnableExport")]
    pub enable_export: Option<bool>,

    #[serde(rename = "ExportFormat")]
    pub export_format: Option<String>,

    #[serde(rename = "DailyRunTime")]
    pub daily_run_time: Option<String>,

    #[serde(rename = "ExportFromTimestamp")]
    pub export_from_timestamp: Option<i64>,

    #[serde(rename = "BatchSize")]
    pub batch_size: Option<i64>,

    #[serde(rename = "DownloadExportResults")]
    pub download_export_results: Option<bool>,

    #[serde(rename = "ChannelBatchSize")]
    pub channel_batch_size: Option<i64>,

    #[serde(rename = "ChannelHistoryBatchSize")]
    pub channel_history_batch_size: Option<i64>,

    #[serde(rename = "GlobalRelaySettings")]
    pub global_relay_settings: Option<GlobalRelayMessageExportSettings>,
}

/// Port of `model.DisplaySettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplaySettings {
    #[serde(rename = "CustomURLSchemes")]
    pub custom_url_schemes: Option<Vec<String>>,

    #[serde(rename = "MaxMarkdownNodes")]
    pub max_markdown_nodes: Option<i64>,
}

/// Port of `model.GuestAccountsSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuestAccountsSettings {
    #[serde(rename = "Enable")]
    pub enable: Option<bool>,

    #[serde(rename = "HideTags")]
    pub hide_tags: Option<bool>,

    #[serde(rename = "AllowEmailAccounts")]
    pub allow_email_accounts: Option<bool>,

    #[serde(rename = "EnforceMultifactorAuthentication")]
    pub enforce_multifactor_authentication: Option<bool>,

    #[serde(rename = "RestrictCreationToDomains")]
    pub restrict_creation_to_domains: Option<String>,

    #[serde(rename = "EnableGuestMagicLink")]
    pub enable_guest_magic_link: Option<bool>,
}

/// Port of `model.ImageProxySettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageProxySettings {
    #[serde(rename = "Enable")]
    pub enable: Option<bool>,

    #[serde(rename = "ImageProxyType")]
    pub image_proxy_type: Option<String>,

    #[serde(rename = "RemoteImageProxyURL")]
    pub remote_image_proxy_url: Option<String>,

    #[serde(rename = "RemoteImageProxyOptions")]
    pub remote_image_proxy_options: Option<String>,
}

/// Port of `model.ImportSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImportSettings {
    #[serde(rename = "Directory")]
    pub directory: Option<String>,

    #[serde(rename = "RetentionDays")]
    pub retention_days: Option<i64>,
}

/// Port of `model.ExportSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportSettings {
    #[serde(rename = "Directory")]
    pub directory: Option<String>,

    #[serde(rename = "RetentionDays")]
    pub retention_days: Option<i64>,
}

/// Port of `model.AccessControlSettings` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccessControlSettings {
    #[serde(rename = "EnableAttributeBasedAccessControl")]
    pub enable_attribute_based_access_control: Option<bool>,

    #[serde(rename = "EnableUserManagedAttributes")]
    pub enable_user_managed_attributes: Option<bool>,

    #[serde(rename = "EnableChannelPolicyIndicators")]
    pub enable_channel_policy_indicators: Option<bool>,

    #[serde(rename = "TrustProxyDeviceIdentityHeader")]
    pub trust_proxy_device_identity_header: Option<bool>,

    #[serde(rename = "EnforceDeviceIDConsistency")]
    pub enforce_device_id_consistency: Option<bool>,

    #[serde(rename = "EnableAccessControlAuditLogging")]
    pub enable_access_control_audit_logging: Option<bool>,

    #[serde(rename = "SyncJobIntervalSeconds")]
    pub sync_job_interval_seconds: Option<i64>,

    #[serde(rename = "AttributeRefreshIntervalSeconds")]
    pub attribute_refresh_interval_seconds: Option<i64>,
}

/// Port of `model.Config` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(rename = "ServiceSettings")]
    pub service_settings: ServiceSettings,

    #[serde(rename = "TeamSettings")]
    pub team_settings: TeamSettings,

    #[serde(rename = "ClientRequirements")]
    pub client_requirements: ClientRequirements,

    #[serde(rename = "SqlSettings")]
    pub sql_settings: SqlSettings,

    #[serde(rename = "LogSettings")]
    pub log_settings: LogSettings,

    #[serde(rename = "ExperimentalAuditSettings")]
    pub experimental_audit_settings: ExperimentalAuditSettings,

    #[serde(rename = "PasswordSettings")]
    pub password_settings: PasswordSettings,

    #[serde(rename = "FileSettings")]
    pub file_settings: FileSettings,

    #[serde(rename = "EmailSettings")]
    pub email_settings: EmailSettings,

    #[serde(rename = "RateLimitSettings")]
    pub rate_limit_settings: RateLimitSettings,

    #[serde(rename = "PrivacySettings")]
    pub privacy_settings: PrivacySettings,

    #[serde(rename = "SupportSettings")]
    pub support_settings: SupportSettings,

    #[serde(rename = "AnnouncementSettings")]
    pub announcement_settings: AnnouncementSettings,

    #[serde(rename = "ThemeSettings")]
    pub theme_settings: ThemeSettings,

    #[serde(rename = "GitLabSettings")]
    pub git_lab_settings: SSOSettings,

    #[serde(rename = "GoogleSettings")]
    pub google_settings: SSOSettings,

    #[serde(rename = "Office365Settings")]
    pub office365_settings: Office365Settings,

    #[serde(rename = "OpenIdSettings")]
    pub open_id_settings: SSOSettings,

    #[serde(rename = "LdapSettings")]
    pub ldap_settings: LdapSettings,

    #[serde(rename = "ComplianceSettings")]
    pub compliance_settings: ComplianceSettings,

    #[serde(rename = "LocalizationSettings")]
    pub localization_settings: LocalizationSettings,

    #[serde(rename = "SamlSettings")]
    pub saml_settings: SamlSettings,

    #[serde(rename = "NativeAppSettings")]
    pub native_app_settings: NativeAppSettings,

    #[serde(rename = "IntuneSettings")]
    pub intune_settings: IntuneSettings,

    #[serde(rename = "CacheSettings")]
    pub cache_settings: CacheSettings,

    #[serde(rename = "ClusterSettings")]
    pub cluster_settings: ClusterSettings,

    #[serde(rename = "MetricsSettings")]
    pub metrics_settings: MetricsSettings,

    #[serde(rename = "ExperimentalSettings")]
    pub experimental_settings: ExperimentalSettings,

    #[serde(rename = "AnalyticsSettings")]
    pub analytics_settings: AnalyticsSettings,

    #[serde(rename = "ElasticsearchSettings")]
    pub elasticsearch_settings: ElasticsearchSettings,

    #[serde(rename = "DataRetentionSettings")]
    pub data_retention_settings: DataRetentionSettings,

    #[serde(rename = "MobileEphemeralModeSettings")]
    pub mobile_ephemeral_mode_settings: MobileEphemeralModeSettings,

    #[serde(rename = "MessageExportSettings")]
    pub message_export_settings: MessageExportSettings,

    #[serde(rename = "JobSettings")]
    pub job_settings: JobSettings,

    #[serde(rename = "PluginSettings")]
    pub plugin_settings: PluginSettings,

    #[serde(rename = "DisplaySettings")]
    pub display_settings: DisplaySettings,

    #[serde(rename = "GuestAccountsSettings")]
    pub guest_accounts_settings: GuestAccountsSettings,

    #[serde(rename = "ImageProxySettings")]
    pub image_proxy_settings: ImageProxySettings,

    #[serde(rename = "CloudSettings")]
    pub cloud_settings: CloudSettings,

    #[serde(rename = "FeatureFlags", skip_serializing_if = "Option::is_none")]
    pub feature_flags: Option<FeatureFlags>,

    #[serde(rename = "ImportSettings")]
    pub import_settings: ImportSettings,

    #[serde(rename = "ExportSettings")]
    pub export_settings: ExportSettings,

    #[serde(rename = "WranglerSettings")]
    pub wrangler_settings: WranglerSettings,

    #[serde(rename = "ConnectedWorkspacesSettings")]
    pub connected_workspaces_settings: ConnectedWorkspacesSettings,

    #[serde(rename = "AccessControlSettings")]
    pub access_control_settings: AccessControlSettings,

    #[serde(rename = "ContentFlaggingSettings")]
    pub content_flagging_settings: ContentFlaggingSettings,

    #[serde(rename = "AutoTranslationSettings")]
    pub auto_translation_settings: AutoTranslationSettings,

    #[serde(rename = "AIRecapSettings")]
    pub ai_recap_settings: AIRecapSettings,
}

/// Port of `model.SanitizeOptions` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SanitizeOptions {
    #[serde(rename = "PartiallyRedactDataSources")]
    pub partially_redact_data_sources: bool,
}

/// Port of `model.FilterTag` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FilterTag {
    #[serde(rename = "TagType")]
    pub tag_type: String,

    #[serde(rename = "TagName")]
    pub tag_name: String,
}

/// Port of `model.ConfigFilterOptions` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigFilterOptions {
    /// Embedded in Go, so `encoding/json` **inlines** its keys rather than nesting them.
    #[serde(flatten)]
    pub get_config_options: GetConfigOptions,

    #[serde(rename = "TagFilters")]
    pub tag_filters: Option<Vec<FilterTag>>,
}

/// Port of `model.GetConfigOptions` (config.go). Field names are the wire keys — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GetConfigOptions {
    #[serde(rename = "RemoveMasked")]
    pub remove_masked: bool,

    #[serde(rename = "RemoveDefaults")]
    pub remove_defaults: bool,
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
    fn cluster_settings_round_trips_the_fixture() {
        assert_fixture_round_trips!(ClusterSettings, "cluster_settings");
    }
    #[test]
    fn elasticsearch_settings_round_trips_the_fixture() {
        assert_fixture_round_trips!(ElasticsearchSettings, "elasticsearch_settings");
    }
    #[test]
    fn config_round_trips_the_fixture() {
        assert_fixture_round_trips!(Config, "config");
    }
}
