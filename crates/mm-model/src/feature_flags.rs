//! Port of `model/feature_flags.go` — the server's feature-flag block.
//!
//! # No `json:` tags: the wire keys are the Go field names
//!
//! `FeatureFlags` is part of `model.Config`, and config structs are marshalled with
//! `encoding/json`'s default — the exported field name verbatim. `ToMap` relies on the same
//! thing through reflection, and the resulting map is what the **ping endpoint and every client**
//! reads. Snake-casing any of these keys breaks feature detection in every client.
//!
//! # `SetDefaults` overwrites unconditionally
//!
//! Unlike every other `SetDefaults` in the tree, this one assigns rather than filling in blanks —
//! it is called on a fresh struct before the flag source is applied. Note also that it does **not**
//! assign every field: `TestFeature` through `RecurringScheduledPosts` are covered, but any field
//! added without a line in `SetDefaults` silently keeps Go's zero value.
//!
//! # Two umbrella dependencies
//!
//! `ChannelPermissionPolicies` and `PolicySimulation` are each meaningless unless
//! `PermissionPolicies` is also on. Go centralises that in two helpers so no call site has to
//! remember; both are ported, and new call sites should use them rather than reading the pair.

use serde::{Deserialize, Serialize};

use crate::utils::StringMap;

/// Port of `model.FeatureFlags` (feature_flags.go:11).
///
/// `TestFeature` and `TestBoolFeature` exist **only for testing**; a boolean flag reads `"on"` or
/// `"true"` as true and anything else as false, which is the flag *source*'s convention rather
/// than this struct's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FeatureFlags {
    #[serde(rename = "TestFeature")]
    pub test_feature: String,

    #[serde(rename = "TestBoolFeature")]
    pub test_bool_feature: bool,

    #[serde(rename = "EnableSharedChannelsDMs")]
    pub enable_shared_channels_dms: bool,

    #[serde(rename = "EnableSyncAllUsersForRemoteCluster")]
    pub enable_sync_all_users_for_remote_cluster: bool,

    #[serde(rename = "AppsEnabled")]
    pub apps_enabled: bool,

    #[serde(rename = "NormalizeLdapDNs")]
    pub normalize_ldap_dns: bool,

    #[serde(rename = "WysiwygEditor")]
    pub wysiwyg_editor: bool,

    #[serde(rename = "EnableExportDirectDownload")]
    pub enable_export_direct_download: bool,

    #[serde(rename = "MoveThreadsEnabled")]
    pub move_threads_enabled: bool,

    #[serde(rename = "NotificationMonitoring")]
    pub notification_monitoring: bool,

    #[serde(rename = "AttributeValueMasking")]
    pub attribute_value_masking: bool,

    #[serde(rename = "PermissionPolicies")]
    pub permission_policies: bool,

    #[serde(rename = "ChannelPermissionPolicies")]
    pub channel_permission_policies: bool,

    #[serde(rename = "PolicySimulation")]
    pub policy_simulation: bool,

    #[serde(rename = "ContentFlagging")]
    pub content_flagging: bool,

    #[serde(rename = "EnableMattermostEntry")]
    pub enable_mattermost_entry: bool,

    #[serde(rename = "MobileSSOCodeExchange")]
    pub mobile_sso_code_exchange: bool,

    #[serde(rename = "EnableShiftEscapeToMarkAllRead")]
    pub enable_shift_escape_to_mark_all_read: bool,

    #[serde(rename = "AutoTranslation")]
    pub auto_translation: bool,

    #[serde(rename = "ClassificationMarkings")]
    pub classification_markings: bool,

    #[serde(rename = "GlobalAttributes")]
    pub global_attributes: bool,

    #[serde(rename = "BurnOnRead")]
    pub burn_on_read: bool,

    #[serde(rename = "EnableAIPluginBridge")]
    pub enable_ai_plugin_bridge: bool,

    #[serde(rename = "EnableAIRecaps")]
    pub enable_ai_recaps: bool,

    #[serde(rename = "IntegratedBoards")]
    pub integrated_boards: bool,

    #[serde(rename = "EnableDocs")]
    pub enable_docs: bool,

    #[serde(rename = "CJKSearch")]
    pub cjk_search: bool,

    #[serde(rename = "AggregatePluginMetrics")]
    pub aggregate_plugin_metrics: bool,

    #[serde(rename = "ManagedChannelCategories")]
    pub managed_channel_categories: bool,

    #[serde(rename = "SessionAttributes")]
    pub session_attributes: bool,

    #[serde(rename = "PostAttributes")]
    pub post_attributes: bool,

    #[serde(rename = "DiscoverableChannels")]
    pub discoverable_channels: bool,

    #[serde(rename = "MobileEphemeralMode")]
    pub mobile_ephemeral_mode: bool,

    #[serde(rename = "PropertyFieldRank")]
    pub property_field_rank: bool,

    #[serde(rename = "TeamMembershipAccessControl")]
    pub team_membership_access_control: bool,

    #[serde(rename = "MmBlocksEnabled")]
    pub mm_blocks_enabled: bool,

    #[serde(rename = "ClusterGracefulDrain")]
    pub cluster_graceful_drain: bool,

    #[serde(rename = "ChannelBookmarks")]
    pub channel_bookmarks: bool,

    #[serde(rename = "EnableConcurrentReact")]
    pub enable_concurrent_react: bool,

    #[serde(rename = "EnableMFIPluginSignaturePublicKey")]
    pub enable_mfi_plugin_signature_public_key: bool,

    #[serde(rename = "RecurringScheduledPosts")]
    pub recurring_scheduled_posts: bool,
}

impl FeatureFlags {
    /// Port of `(*FeatureFlags).SetDefaults` (feature_flags.go:160).
    pub fn set_defaults(&mut self) {
        self.test_feature = "off".to_string();
        self.test_bool_feature = false;
        self.enable_shared_channels_dms = false;
        self.enable_sync_all_users_for_remote_cluster = false;
        self.apps_enabled = false;
        self.normalize_ldap_dns = false;
        self.wysiwyg_editor = false;
        self.enable_export_direct_download = false;
        self.move_threads_enabled = false;
        self.notification_monitoring = true;
        self.attribute_value_masking = true;
        self.permission_policies = true;
        self.channel_permission_policies = true;
        self.policy_simulation = true;
        self.content_flagging = true;
        self.enable_mattermost_entry = true;
        self.mobile_sso_code_exchange = false;
        self.enable_shift_escape_to_mark_all_read = false;
        self.auto_translation = true;
        self.classification_markings = true;
        self.burn_on_read = true;
        self.enable_ai_plugin_bridge = false;
        self.enable_ai_recaps = false;
        self.integrated_boards = false;
        self.enable_docs = false;
        self.cjk_search = true;
        self.aggregate_plugin_metrics = false;
        self.managed_channel_categories = false;
        self.session_attributes = false;
        self.post_attributes = false;
        self.discoverable_channels = false;
        self.mobile_ephemeral_mode = false;
        self.property_field_rank = true;
        self.team_membership_access_control = true;
        self.mm_blocks_enabled = true;
        self.cluster_graceful_drain = true;
        self.channel_bookmarks = true;
        self.enable_concurrent_react = false;
        self.enable_mfi_plugin_signature_public_key = true;
        self.recurring_scheduled_posts = false;
    }

    /// Port of `(*FeatureFlags).IsChannelPermissionPoliciesEnabled` (feature_flags.go:230).
    ///
    /// **Both** flags must be on: turning the umbrella off disables the sub-feature even when its
    /// own flag is set.
    pub fn is_channel_permission_policies_enabled(&self) -> bool {
        self.permission_policies && self.channel_permission_policies
    }

    /// Port of `(*FeatureFlags).IsPolicySimulationEnabled` (feature_flags.go:241).
    pub fn is_policy_simulation_enabled(&self) -> bool {
        self.permission_policies && self.policy_simulation
    }

    /// Port of `(*FeatureFlags).ToMap` (feature_flags.go:247).
    ///
    /// Go walks the struct with reflection; the keys are therefore the **field names**, and a bool
    /// is rendered by `strconv.FormatBool` as `"true"`/`"false"`. Enumerated by hand here — there
    /// is no reflection — which means a new flag must be added in **two** places, this map and the
    /// struct. That is the cost of not having reflection, and it is worth stating plainly.
    pub fn to_map(&self) -> StringMap {
        let mut out = StringMap::new();
        let mut put = |key: &str, value: &str| {
            out.insert(key.to_string(), value.to_string());
        };

        put("TestFeature", &self.test_feature);
        put(
            "TestBoolFeature",
            if self.test_bool_feature {
                "true"
            } else {
                "false"
            },
        );
        put(
            "EnableSharedChannelsDMs",
            if self.enable_shared_channels_dms {
                "true"
            } else {
                "false"
            },
        );
        put(
            "EnableSyncAllUsersForRemoteCluster",
            if self.enable_sync_all_users_for_remote_cluster {
                "true"
            } else {
                "false"
            },
        );
        put(
            "AppsEnabled",
            if self.apps_enabled { "true" } else { "false" },
        );
        put(
            "NormalizeLdapDNs",
            if self.normalize_ldap_dns {
                "true"
            } else {
                "false"
            },
        );
        put(
            "WysiwygEditor",
            if self.wysiwyg_editor { "true" } else { "false" },
        );
        put(
            "EnableExportDirectDownload",
            if self.enable_export_direct_download {
                "true"
            } else {
                "false"
            },
        );
        put(
            "MoveThreadsEnabled",
            if self.move_threads_enabled {
                "true"
            } else {
                "false"
            },
        );
        put(
            "NotificationMonitoring",
            if self.notification_monitoring {
                "true"
            } else {
                "false"
            },
        );
        put(
            "AttributeValueMasking",
            if self.attribute_value_masking {
                "true"
            } else {
                "false"
            },
        );
        put(
            "PermissionPolicies",
            if self.permission_policies {
                "true"
            } else {
                "false"
            },
        );
        put(
            "ChannelPermissionPolicies",
            if self.channel_permission_policies {
                "true"
            } else {
                "false"
            },
        );
        put(
            "PolicySimulation",
            if self.policy_simulation {
                "true"
            } else {
                "false"
            },
        );
        put(
            "ContentFlagging",
            if self.content_flagging {
                "true"
            } else {
                "false"
            },
        );
        put(
            "EnableMattermostEntry",
            if self.enable_mattermost_entry {
                "true"
            } else {
                "false"
            },
        );
        put(
            "MobileSSOCodeExchange",
            if self.mobile_sso_code_exchange {
                "true"
            } else {
                "false"
            },
        );
        put(
            "EnableShiftEscapeToMarkAllRead",
            if self.enable_shift_escape_to_mark_all_read {
                "true"
            } else {
                "false"
            },
        );
        put(
            "AutoTranslation",
            if self.auto_translation {
                "true"
            } else {
                "false"
            },
        );
        put(
            "ClassificationMarkings",
            if self.classification_markings {
                "true"
            } else {
                "false"
            },
        );
        put(
            "GlobalAttributes",
            if self.global_attributes {
                "true"
            } else {
                "false"
            },
        );
        put(
            "BurnOnRead",
            if self.burn_on_read { "true" } else { "false" },
        );
        put(
            "EnableAIPluginBridge",
            if self.enable_ai_plugin_bridge {
                "true"
            } else {
                "false"
            },
        );
        put(
            "EnableAIRecaps",
            if self.enable_ai_recaps {
                "true"
            } else {
                "false"
            },
        );
        put(
            "IntegratedBoards",
            if self.integrated_boards {
                "true"
            } else {
                "false"
            },
        );
        put(
            "EnableDocs",
            if self.enable_docs { "true" } else { "false" },
        );
        put("CJKSearch", if self.cjk_search { "true" } else { "false" });
        put(
            "AggregatePluginMetrics",
            if self.aggregate_plugin_metrics {
                "true"
            } else {
                "false"
            },
        );
        put(
            "ManagedChannelCategories",
            if self.managed_channel_categories {
                "true"
            } else {
                "false"
            },
        );
        put(
            "SessionAttributes",
            if self.session_attributes {
                "true"
            } else {
                "false"
            },
        );
        put(
            "PostAttributes",
            if self.post_attributes {
                "true"
            } else {
                "false"
            },
        );
        put(
            "DiscoverableChannels",
            if self.discoverable_channels {
                "true"
            } else {
                "false"
            },
        );
        put(
            "MobileEphemeralMode",
            if self.mobile_ephemeral_mode {
                "true"
            } else {
                "false"
            },
        );
        put(
            "PropertyFieldRank",
            if self.property_field_rank {
                "true"
            } else {
                "false"
            },
        );
        put(
            "TeamMembershipAccessControl",
            if self.team_membership_access_control {
                "true"
            } else {
                "false"
            },
        );
        put(
            "MmBlocksEnabled",
            if self.mm_blocks_enabled {
                "true"
            } else {
                "false"
            },
        );
        put(
            "ClusterGracefulDrain",
            if self.cluster_graceful_drain {
                "true"
            } else {
                "false"
            },
        );
        put(
            "ChannelBookmarks",
            if self.channel_bookmarks {
                "true"
            } else {
                "false"
            },
        );
        put(
            "EnableConcurrentReact",
            if self.enable_concurrent_react {
                "true"
            } else {
                "false"
            },
        );
        put(
            "EnableMFIPluginSignaturePublicKey",
            if self.enable_mfi_plugin_signature_public_key {
                "true"
            } else {
                "false"
            },
        );
        put(
            "RecurringScheduledPosts",
            if self.recurring_scheduled_posts {
                "true"
            } else {
                "false"
            },
        );

        out
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_sweep_models.json"
        ))
        .expect("behaviour_sweep_models.json is generated by reference/dump")
    }

    /// `SetDefaults` is a wall of per-flag defaults with no pattern to infer, and `ToMap` decides
    /// which of them the client sees and how a bool is spelled. Both are asserted whole: the
    /// oracle's map is the complete answer, so a flag added, dropped, renamed or defaulted
    /// differently fails here rather than in a client months later.
    #[test]
    fn set_defaults_and_to_map_match_go() {
        let oracle = oracle();
        let corpus = &oracle["feature_flags_to_map"];

        let mut flags = FeatureFlags::default();
        flags.set_defaults();

        let expected: crate::utils::StringMap =
            serde_json::from_value(corpus["defaults"].clone()).unwrap();
        assert_eq!(flags.to_map(), expected, "ToMap after SetDefaults");

        let zero: crate::utils::StringMap =
            serde_json::from_value(corpus["zero_value_map"].clone()).unwrap();
        assert_eq!(
            FeatureFlags::default().to_map(),
            zero,
            "ToMap on a zero value"
        );

        assert_eq!(
            flags.test_feature,
            corpus["defaults_test_feature"].as_str().unwrap(),
            "TestFeature is a string flag, not a bool"
        );
        assert_eq!(
            flags.cluster_graceful_drain,
            corpus["defaults_cluster_graceful_drain"].as_bool().unwrap()
        );

        // The two umbrella-dependent helpers: each reads its own flag **and** PermissionPolicies,
        // so turning the umbrella off must turn both off regardless of the specific flags.
        assert_eq!(
            flags.is_channel_permission_policies_enabled(),
            corpus["defaults_channel_policies"].as_bool().unwrap()
        );
        assert_eq!(
            flags.is_policy_simulation_enabled(),
            corpus["defaults_policy_simulation"].as_bool().unwrap()
        );

        let mut umbrella_off = flags.clone();
        umbrella_off.permission_policies = false;
        assert_eq!(
            umbrella_off.is_channel_permission_policies_enabled(),
            corpus["umbrella_off_channel_policies"].as_bool().unwrap()
        );
        assert_eq!(
            umbrella_off.is_policy_simulation_enabled(),
            corpus["umbrella_off_policy_simulation"].as_bool().unwrap()
        );
    }
}
