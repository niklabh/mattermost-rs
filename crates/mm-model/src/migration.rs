//! Port of `model/migration.go` — the keys under which completed migrations are recorded.
//!
//! Every value is a row in `Systems`: the server writes the key once the migration has run and
//! then never runs it again. **The constant names and the values disagree often enough that
//! reading one and assuming the other is a real risk** — `AdvancedPermissionsMigrationKey` is
//! `AdvancedPermissionsMigrationComplete` (the only CamelCase value in the file);
//! `MigrationKeyAddManageSharedChannelPermissions` drops its `add_`;
//! `MigrationKeyAddPlayboosksManageRolesPermissions` has the typo in the *name*, not the value.
//!
//! These are **generated** from the Go source rather than transcribed, precisely because a
//! 63-entry constant block is where a hand-copy silently loses one.

/// Port of `model.AdvancedPermissionsMigrationKey` (migration.go).
pub const ADVANCED_PERMISSIONS_MIGRATION_KEY: &str = "AdvancedPermissionsMigrationComplete";
/// Port of `model.MigrationKeyAdvancedPermissionsPhase2` (migration.go).
pub const MIGRATION_KEY_ADVANCED_PERMISSIONS_PHASE2: &str =
    "migration_advanced_permissions_phase_2";
/// Port of `model.MigrationKeyEmojiPermissionsSplit` (migration.go).
pub const MIGRATION_KEY_EMOJI_PERMISSIONS_SPLIT: &str = "emoji_permissions_split";
/// Port of `model.MigrationKeyWebhookPermissionsSplit` (migration.go).
pub const MIGRATION_KEY_WEBHOOK_PERMISSIONS_SPLIT: &str = "webhook_permissions_split";
/// Port of `model.MigrationKeyIntegrationsOwnPermissions` (migration.go).
pub const MIGRATION_KEY_INTEGRATIONS_OWN_PERMISSIONS: &str = "integrations_own_permissions";
/// Port of `model.MigrationKeyListJoinPublicPrivateTeams` (migration.go).
pub const MIGRATION_KEY_LIST_JOIN_PUBLIC_PRIVATE_TEAMS: &str = "list_join_public_private_teams";
/// Port of `model.MigrationKeyRemovePermanentDeleteUser` (migration.go).
pub const MIGRATION_KEY_REMOVE_PERMANENT_DELETE_USER: &str = "remove_permanent_delete_user";
/// Port of `model.MigrationKeyAddBotPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_BOT_PERMISSIONS: &str = "add_bot_permissions";
/// Port of `model.MigrationKeyApplyChannelManageDeleteToChannelUser` (migration.go).
pub const MIGRATION_KEY_APPLY_CHANNEL_MANAGE_DELETE_TO_CHANNEL_USER: &str =
    "apply_channel_manage_delete_to_channel_user";
/// Port of `model.MigrationKeyRemoveChannelManageDeleteFromTeamUser` (migration.go).
pub const MIGRATION_KEY_REMOVE_CHANNEL_MANAGE_DELETE_FROM_TEAM_USER: &str =
    "remove_channel_manage_delete_from_team_user";
/// Port of `model.MigrationKeyViewMembersNewPermission` (migration.go).
pub const MIGRATION_KEY_VIEW_MEMBERS_NEW_PERMISSION: &str = "view_members_new_permission";
/// Port of `model.MigrationKeyAddManageGuestsPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_MANAGE_GUESTS_PERMISSIONS: &str = "add_manage_guests_permissions";
/// Port of `model.MigrationKeyChannelModerationsPermissions` (migration.go).
pub const MIGRATION_KEY_CHANNEL_MODERATIONS_PERMISSIONS: &str = "channel_moderations_permissions";
/// Port of `model.MigrationKeyAddUseGroupMentionsPermission` (migration.go).
pub const MIGRATION_KEY_ADD_USE_GROUP_MENTIONS_PERMISSION: &str =
    "add_use_group_mentions_permission";
/// Port of `model.MigrationKeyAddSystemConsolePermissions` (migration.go).
pub const MIGRATION_KEY_ADD_SYSTEM_CONSOLE_PERMISSIONS: &str = "add_system_console_permissions";
/// Port of `model.MigrationKeySidebarCategoriesPhase2` (migration.go).
pub const MIGRATION_KEY_SIDEBAR_CATEGORIES_PHASE2: &str = "migration_sidebar_categories_phase_2";
/// Port of `model.MigrationKeyAddConvertChannelPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_CONVERT_CHANNEL_PERMISSIONS: &str = "add_convert_channel_permissions";
/// Port of `model.MigrationKeyAddSystemRolesPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_SYSTEM_ROLES_PERMISSIONS: &str = "add_system_roles_permissions";
/// Port of `model.MigrationKeyAddBillingPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_BILLING_PERMISSIONS: &str = "add_billing_permissions";
/// Port of `model.MigrationKeyAddManageSharedChannelPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_MANAGE_SHARED_CHANNEL_PERMISSIONS: &str =
    "manage_shared_channel_permissions";
/// Port of `model.MigrationKeyAddManageSecureConnectionsPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_MANAGE_SECURE_CONNECTIONS_PERMISSIONS: &str =
    "manage_secure_connections_permissions";
/// Port of `model.MigrationKeyAddDownloadComplianceExportResults` (migration.go).
pub const MIGRATION_KEY_ADD_DOWNLOAD_COMPLIANCE_EXPORT_RESULTS: &str =
    "download_compliance_export_results";
/// Port of `model.MigrationKeyAddComplianceSubsectionPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_COMPLIANCE_SUBSECTION_PERMISSIONS: &str =
    "compliance_subsection_permissions";
/// Port of `model.MigrationKeyAddExperimentalSubsectionPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_EXPERIMENTAL_SUBSECTION_PERMISSIONS: &str =
    "experimental_subsection_permissions";
/// Port of `model.MigrationKeyAddAuthenticationSubsectionPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_AUTHENTICATION_SUBSECTION_PERMISSIONS: &str =
    "authentication_subsection_permissions";
/// Port of `model.MigrationKeyAddSiteSubsectionPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_SITE_SUBSECTION_PERMISSIONS: &str = "site_subsection_permissions";
/// Port of `model.MigrationKeyAddEnvironmentSubsectionPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_ENVIRONMENT_SUBSECTION_PERMISSIONS: &str =
    "environment_subsection_permissions";
/// Port of `model.MigrationKeyAddReportingSubsectionPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_REPORTING_SUBSECTION_PERMISSIONS: &str =
    "reporting_subsection_permissions";
/// Port of `model.MigrationKeyAddTestEmailAncillaryPermission` (migration.go).
pub const MIGRATION_KEY_ADD_TEST_EMAIL_ANCILLARY_PERMISSION: &str =
    "test_email_ancillary_permission";
/// Port of `model.MigrationKeyAddAboutSubsectionPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_ABOUT_SUBSECTION_PERMISSIONS: &str = "about_subsection_permissions";
/// Port of `model.MigrationKeyAddIntegrationsSubsectionPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_INTEGRATIONS_SUBSECTION_PERMISSIONS: &str =
    "integrations_subsection_permissions";
/// Port of `model.MigrationKeyAddPlaybooksPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_PLAYBOOKS_PERMISSIONS: &str = "playbooks_permissions";
/// Port of `model.MigrationKeyAddCustomUserGroupsPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_CUSTOM_USER_GROUPS_PERMISSIONS: &str = "custom_groups_permissions";
/// Port of `model.MigrationKeyAddPlayboosksManageRolesPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_PLAYBOOSKS_MANAGE_ROLES_PERMISSIONS: &str = "playbooks_manage_roles";
/// Port of `model.MigrationKeyAddProductsBoardsPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_PRODUCTS_BOARDS_PERMISSIONS: &str = "products_boards";
/// Port of `model.MigrationKeyAddCustomUserGroupsPermissionRestore` (migration.go).
pub const MIGRATION_KEY_ADD_CUSTOM_USER_GROUPS_PERMISSION_RESTORE: &str =
    "custom_groups_permission_restore";
/// Port of `model.MigrationKeyAddReadChannelContentPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_READ_CHANNEL_CONTENT_PERMISSIONS: &str =
    "read_channel_content_permissions";
/// Port of `model.MigrationKeyS3Path` (migration.go).
pub const MIGRATION_KEY_S3_PATH: &str = "s3_path_migration";
/// Port of `model.MigrationKeyDeleteEmptyDrafts` (migration.go).
pub const MIGRATION_KEY_DELETE_EMPTY_DRAFTS: &str = "delete_empty_drafts_migration";
/// Port of `model.MigrationKeyDeleteOrphanDrafts` (migration.go).
pub const MIGRATION_KEY_DELETE_ORPHAN_DRAFTS: &str = "delete_orphan_drafts_migration";
/// Port of `model.MigrationKeyAddIPFilteringPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_IP_FILTERING_PERMISSIONS: &str = "add_ip_filtering_permissions";
/// Port of `model.MigrationKeyAddOutgoingOAuthConnectionsPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_OUTGOING_OAUTH_CONNECTIONS_PERMISSIONS: &str =
    "add_outgoing_oauth_connections_permissions";
/// Port of `model.MigrationKeyAddChannelBookmarksPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_CHANNEL_BOOKMARKS_PERMISSIONS: &str =
    "add_channel_bookmarks_permissions";
/// Port of `model.MigrationKeyDeleteDmsPreferences` (migration.go).
pub const MIGRATION_KEY_DELETE_DMS_PREFERENCES: &str = "delete_dms_preferences_migration";
/// Port of `model.MigrationKeyAddManageJobAncillaryPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_MANAGE_JOB_ANCILLARY_PERMISSIONS: &str =
    "add_manage_jobs_ancillary_permissions";
/// Port of `model.MigrationKeyAddUploadFilePermission` (migration.go).
pub const MIGRATION_KEY_ADD_UPLOAD_FILE_PERMISSION: &str = "add_upload_file_permission";
/// Port of `model.RestrictAccessToChannelConversionToPublic` (migration.go).
pub const RESTRICT_ACCESS_TO_CHANNEL_CONVERSION_TO_PUBLIC: &str =
    "restrict_access_to_channel_conversion_to_public_permissions";
/// Port of `model.MigrationKeyFixReadAuditsPermission` (migration.go).
pub const MIGRATION_KEY_FIX_READ_AUDITS_PERMISSION: &str = "fix_read_audits_permission";
/// Port of `model.MigrationRemoveGetAnalyticsPermission` (migration.go).
pub const MIGRATION_REMOVE_GET_ANALYTICS_PERMISSION: &str = "remove_get_analytics_permission";
/// Port of `model.MigrationAddSysconsoleMobileSecurityPermission` (migration.go).
pub const MIGRATION_ADD_SYSCONSOLE_MOBILE_SECURITY_PERMISSION: &str =
    "add_sysconsole_mobile_security_permission";
/// Port of `model.MigrationKeyAddChannelBannerPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_CHANNEL_BANNER_PERMISSIONS: &str = "add_channel_banner_permissions";
/// Port of `model.MigrationKeyAddChannelAccessRulesPermission` (migration.go).
pub const MIGRATION_KEY_ADD_CHANNEL_ACCESS_RULES_PERMISSION: &str =
    "add_channel_access_rules_permission";
/// Port of `model.MigrationKeyAddChannelAutoTranslationPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_CHANNEL_AUTO_TRANSLATION_PERMISSIONS: &str =
    "add_channel_auto_translation_permissions";
/// Port of `model.MigrationKeyAddTeamAccessRulesPermission` (migration.go).
pub const MIGRATION_KEY_ADD_TEAM_ACCESS_RULES_PERMISSION: &str = "add_team_access_rules_permission";
/// Port of `model.MigrationKeyAddSecureConnectionManagerPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_SECURE_CONNECTION_MANAGER_PERMISSIONS: &str =
    "secure_connection_manager_permissions";
/// Port of `model.MigrationKeyAddSharedChannelManagerPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_SHARED_CHANNEL_MANAGER_PERMISSIONS: &str =
    "system_shared_channel_manager_permissions";
/// Port of `model.MigrationKeyRestoreManageOAuthPermission` (migration.go).
pub const MIGRATION_KEY_RESTORE_MANAGE_OAUTH_PERMISSION: &str = "restore_manage_oauth_permission";
/// Port of `model.MigrationKeyAccessControlPolicyV0_3` (migration.go).
pub const MIGRATION_KEY_ACCESS_CONTROL_POLICY_V0_3: &str = "access_control_policy_v0_3_migration";
/// Port of `model.MigrationKeyAddManageAgentPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_MANAGE_AGENT_PERMISSIONS: &str = "add_manage_agent_permissions";
/// Port of `model.MigrationKeyAddEditFileAttachmentPermission` (migration.go).
pub const MIGRATION_KEY_ADD_EDIT_FILE_ATTACHMENT_PERMISSION: &str =
    "add_edit_file_attachment_permission";
/// Port of `model.MigrationKeyAddDiscoverableChannelPermissions` (migration.go).
pub const MIGRATION_KEY_ADD_DISCOVERABLE_CHANNEL_PERMISSIONS: &str =
    "add_discoverable_channel_permissions";
